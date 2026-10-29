// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! USB Attached SCSI (UAS, protocol 62h), as USB 3 disks and the bridges
//! in USB enclosures carry SCSI commands, many at once (Phase 11b, ADR
//! 0162).
//!
//! A UAS interface has four bulk pipes, each named by the pipe usage
//! descriptor after its endpoint's ([`find_pipes`]). Each command goes out
//! on the command pipe in a Command IU (information unit) with a tag of
//! its own; the device answers on the status pipe with a Sense IU, which
//! holds the command's SCSI status and, if it failed, its sense data, so
//! no REQUEST SENSE follows; and the data moves on the data-in or the
//! data-out pipe. At SuperSpeed the status and data pipes have streams,
//! and a command's status and data go on the stream its tag names: the
//! host queues them before the command, and the device moves each when it
//! is ready, in whatever order it likes. Below SuperSpeed there are no
//! streams: the device says which command's data it is ready to move with
//! a Read Ready or Write Ready IU on the status pipe, and the host then
//! queues that data, and another status.
//!
//! [`Uas`] runs the commands as Linux's `uas` driver does, without knowing
//! how the host controller carries them: it says which transfers to start
//! or stop ([`Action`]), is told how each ended ([`Uas::ended`]), and hands
//! back each command as it finishes ([`Uas::next_done`]). As on Linux, it
//! sends no task management: a command that goes unanswered, a pipe that
//! fails, or an IU it cannot make sense of has the device reset (its port
//! reset, its endpoints and streams set up again), which ends every
//! command in flight, to go again ([`Uas::reset`]). [`OneAtATime`] carries
//! one command at a time over it, as a [`Transport`], for the probe and
//! the partition table.

use radix_scsi::{Cdb, Data, Reply, Sense, Transport, TransportError};

use crate::{EndpointDescriptor, Interface, TransferType};

use super::PipeError;

#[cfg(test)]
mod tests;

/// The pipe usage descriptor's type (UAS 1.0, 5.3.3.1).
pub const PIPE_USAGE: u8 = 0x24;
/// A Command IU's length, with a CDB of up to 16 bytes.
pub const COMMAND_LENGTH: usize = 32;
/// How much of a status the host takes: a Sense IU with 96 bytes of sense
/// data, Linux's `struct sense_iu`.
pub const STATUS_LENGTH: usize = 112;
/// The most tags, and so commands in flight, an interface is given:
/// streams 1 to 31 at SuperSpeed (`radix_xhci::ring::MAX_STREAMS`), and
/// Linux's 32 below it.
pub const MAX_TAGS: usize = 32;
/// The queue depth of an interface without streams: Linux's.
pub const DEPTH_WITHOUT_STREAMS: usize = 32;

/// The IUs' IDs (UAS 1.0, 6.2).
mod iu {
    pub const COMMAND: u8 = 0x01;
    pub const SENSE: u8 = 0x03;
    pub const RESPONSE: u8 = 0x04;
    pub const READ_READY: u8 = 0x06;
    pub const WRITE_READY: u8 = 0x07;
}

/// A Response IU's codes (UAS 1.0, 6.2.5) that mean more than a failure.
mod response {
    /// The command was sent to a logical unit the device does not have.
    pub const INCORRECT_LUN: u8 = 0x09;
}

/// SCSI statuses (SAM-5, 5.3.1).
mod status {
    pub const GOOD: u8 = 0x00;
    pub const CHECK_CONDITION: u8 = 0x02;
    pub const CONDITION_MET: u8 = 0x04;
}

/// A UAS interface's pipes, in the order their pipe IDs number them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pipe {
    /// Command IUs, to the device.
    Command,
    /// Sense, Response and Ready IUs, from it.
    Status,
    /// Data from the device.
    DataIn,
    /// Data to it.
    DataOut,
}

impl Pipe {
    /// Every pipe, by pipe ID.
    pub const ALL: [Pipe; 4] = [Pipe::Command, Pipe::Status, Pipe::DataIn, Pipe::DataOut];

    /// Its place in [`Pipe::ALL`], one less than its pipe ID.
    pub fn index(self) -> usize {
        self as usize
    }

    /// The data pipe that moves data which way.
    fn data(into_host: bool) -> Pipe {
        if into_host { Pipe::DataIn } else { Pipe::DataOut }
    }
}

/// The endpoints of a UAS setting `interface`, in [`Pipe::ALL`]'s order,
/// as the pipe usage descriptor after each says; `None` unless it has all
/// four, bulk and each the way its pipe goes, as Linux's
/// `uas_find_endpoints` asks.
pub fn find_pipes(interface: &Interface<'_>) -> Option<[EndpointDescriptor; 4]> {
    let mut pipes: [Option<EndpointDescriptor>; 4] = [None; 4];
    let mut last: Option<EndpointDescriptor> = None;
    for (kind, bytes) in interface.descriptors() {
        match kind {
            crate::descriptor::ENDPOINT => last = EndpointDescriptor::parse(bytes),
            crate::descriptor::SUPERSPEED_ENDPOINT_COMPANION if bytes.len() >= 4 => {
                if let Some(endpoint) = &mut last {
                    endpoint.max_burst = bytes[2];
                    endpoint.companion_attributes = bytes[3];
                }
            }
            PIPE_USAGE if bytes.len() >= 3 => {
                let id = usize::from(bytes[2]);
                if let (1..=4, Some(endpoint)) = (id, last) {
                    pipes[id - 1] = Some(endpoint);
                }
            }
            _ => {}
        }
    }
    let mut found = [EndpointDescriptor::default(); 4];
    for (at, pipe) in Pipe::ALL.into_iter().enumerate() {
        let endpoint = pipes[at]?;
        let into_host = matches!(pipe, Pipe::Status | Pipe::DataIn);
        if endpoint.transfer_type() != TransferType::Bulk || endpoint.is_in() != into_host {
            return None;
        }
        found[at] = endpoint;
    }
    Some(found)
}

/// A Command IU (UAS 1.0, 6.2.2): `cdb` to logical unit `lun`, with
/// `tag`, as a simple task.
pub fn command_iu(tag: u16, lun: u8, cdb: &Cdb) -> [u8; COMMAND_LENGTH] {
    let mut bytes = [0u8; COMMAND_LENGTH];
    bytes[0] = iu::COMMAND;
    bytes[2..4].copy_from_slice(&tag.to_be_bytes());
    // The LUN in SAM's single level format, as Linux's int_to_scsilun
    // writes one below 256.
    bytes[9] = lun;
    let cdb = cdb.bytes();
    bytes[16..16 + cdb.len()].copy_from_slice(cdb);
    bytes
}

/// What the host controller is to do for [`Uas`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Start a transfer on a pipe, numbered `n`: the Command IU of the
    /// command with tag `n`, which [`Uas::start`] gave; a status into
    /// status buffer `n`, of [`STATUS_LENGTH`] bytes, on stream `n` with
    /// streams; or the data of the command with tag `n`, on stream `n`
    /// with streams.
    Start(Pipe, u16),
    /// Stop the transfer started as `Start(pipe, n)`, which still ends
    /// ([`Uas::ended`]): a command that failed leaves its data waiting.
    Cancel(Pipe, u16),
}

/// The most actions waiting: four for each tag.
const ACTIONS: usize = 4 * MAX_TAGS;

/// Actions in the order they are to be carried out.
struct Actions {
    items: [Action; ACTIONS],
    head: usize,
    len: usize,
}

impl Actions {
    const EMPTY: Actions =
        Actions { items: [Action::Cancel(Pipe::Command, 0); ACTIONS], head: 0, len: 0 };

    /// False if there is no room, which each tag's few never fill.
    fn push(&mut self, action: Action) -> bool {
        if self.len == ACTIONS {
            return false;
        }
        self.items[(self.head + self.len) % ACTIONS] = action;
        self.len += 1;
        true
    }

    fn pop(&mut self) -> Option<Action> {
        if self.len == 0 {
            return None;
        }
        let action = self.items[self.head];
        self.head = (self.head + 1) % ACTIONS;
        self.len -= 1;
        Some(action)
    }

    fn clear(&mut self) {
        self.len = 0;
    }
}

/// Why the device needs a reset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Broken {
    NoRoom,
    RefusedCommand,
    StatusPipe,
    DataPipe,
    ShortStatus,
    OtherStream,
    RefusedIu,
    BadStatus,
    TimedOut,
    ResetFailed,
}

impl Broken {
    fn why(self) -> &'static str {
        match self {
            Broken::NoRoom => "it had more to do than room for it",
            Broken::RefusedCommand => "it refused a command",
            Broken::StatusPipe => "its status pipe failed",
            Broken::DataPipe => "its data pipe failed",
            Broken::ShortStatus => "it sent a status too short to read",
            Broken::OtherStream => "it answered a command on another's stream",
            Broken::RefusedIu => "it refused a command's IU",
            Broken::BadStatus => "it sent a status that is not valid",
            Broken::TimedOut => "it did not answer in time",
            Broken::ResetFailed => "its reset failed",
        }
    }
}

/// How a command ended, or that it has not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    /// Its status has not come.
    Pending,
    /// It worked.
    Good,
    /// It failed, with its sense data if it had some.
    Failed(Option<Sense>),
    /// The device was reset, for this.
    Reset(Broken),
    /// The device is gone.
    Gone,
}

/// A command in flight, under its tag: kept small, so that [`Uas`] fits
/// a kernel stack frame.
#[derive(Clone, Copy, Debug)]
struct Slot {
    used: bool,
    /// Which way its data moves, and how much: none for 0.
    into_host: bool,
    length: u32,
    /// Its Command IU's transfer is in flight.
    command_running: bool,
    /// Its data's transfer is in flight.
    data_running: bool,
    /// Its data's transfer was stopped.
    data_cancelled: bool,
    /// How much of its data moved, once its transfer ended.
    moved: u32,
    ending: Ending,
}

impl Slot {
    const FREE: Slot = Slot {
        used: false,
        into_host: false,
        length: 0,
        command_running: false,
        data_running: false,
        data_cancelled: false,
        moved: 0,
        ending: Ending::Pending,
    };
}

/// A UAS interface's commands in flight, and what their transfers wait
/// for.
pub struct Uas {
    /// Whether its status and data pipes have streams.
    streams: bool,
    /// The most commands in flight at once, with tags 1 to this.
    depth: usize,
    /// The commands in flight, by tag less one.
    slots: [Slot; MAX_TAGS],
    /// The status buffers (streams, with streams) whose transfers are in
    /// flight, a bit each, bit `n - 1` for buffer `n`.
    statuses: u32,
    /// What the host controller is to do next.
    actions: Actions,
    /// Why the device needs a reset, if it does: nothing more starts
    /// until [`Uas::reset`].
    broken: Option<Broken>,
    /// The device is gone.
    gone: bool,
}

impl Uas {
    /// An interface whose status and data pipes have `streams` streams
    /// each, or none: it takes two commands fewer than it has tags for
    /// (Linux's `can_queue`), at least one: its streams' worth with
    /// streams, up to [`MAX_TAGS`], and [`DEPTH_WITHOUT_STREAMS`] without.
    pub fn new(streams: Option<usize>) -> Uas {
        let tags = streams.map_or(DEPTH_WITHOUT_STREAMS, |n| n.min(MAX_TAGS));
        Uas {
            streams: streams.is_some(),
            depth: tags.saturating_sub(2).max(1),
            slots: [Slot::FREE; MAX_TAGS],
            statuses: 0,
            actions: Actions::EMPTY,
            broken: None,
            gone: false,
        }
    }

    /// The most commands it takes in flight at once.
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Whether its pipes have streams.
    pub fn streams(&self) -> bool {
        self.streams
    }

    /// How many commands are in flight.
    pub fn in_flight(&self) -> usize {
        self.slots.iter().filter(|s| s.used).count()
    }

    /// The lowest tag free, if a command can start now.
    pub fn free_tag(&self) -> Option<u16> {
        if self.broken.is_some() || self.gone {
            return None;
        }
        let at = self.slots[..self.depth].iter().position(|s| !s.used)?;
        Some(at as u16 + 1)
    }

    /// Why the device needs a reset, if it does.
    pub fn broken(&self) -> Option<&'static str> {
        self.broken.map(Broken::why)
    }

    /// Whether the device is gone.
    pub fn is_gone(&self) -> bool {
        self.gone
    }

    fn slot(&mut self, tag: u16) -> Option<&mut Slot> {
        let at = usize::from(tag).checked_sub(1)?;
        self.slots.get_mut(at).filter(|s| s.used)
    }

    /// The device needs a reset, for `why`: nothing more is started.
    fn break_with(&mut self, why: Broken) {
        if self.broken.is_none() {
            self.broken = Some(why);
        }
        self.actions.clear();
    }

    fn push(&mut self, action: Action) {
        if !self.actions.push(action) {
            self.break_with(Broken::NoRoom);
        }
    }

    /// Starts `cdb` to logical unit `lun` with tag `tag`, with data of
    /// `data`'s direction (into the host) and length: says which transfers
    /// to start, its data's last if it waits for a Ready IU. Returns the
    /// command's IU, which its [`Action::Start`] on the command pipe moves;
    /// `None` if the tag is past its depth or in use, or the device needs
    /// a reset or is gone.
    pub fn start(
        &mut self,
        tag: u16,
        lun: u8,
        cdb: &Cdb,
        data: Option<(bool, u32)>,
    ) -> Option<[u8; COMMAND_LENGTH]> {
        let at = usize::from(tag).wrapping_sub(1);
        if at >= self.depth || self.slots[at].used || self.broken.is_some() || self.gone {
            return None;
        }
        let (into_host, length) = data.unwrap_or((false, 0));
        let Some(status) = self.free_status(tag) else {
            // A status buffer for each command in flight, so never.
            self.break_with(Broken::NoRoom);
            return None;
        };
        self.slots[at] =
            Slot { used: true, into_host, length, command_running: true, ..Slot::FREE };
        // The status and data first, so that the device finds them
        // waiting when the command comes (as Linux queues them).
        self.start_status(status);
        if length > 0 && self.streams {
            self.start_data(tag, into_host);
        }
        self.push(Action::Start(Pipe::Command, tag));
        Some(command_iu(tag, lun, cdb))
    }

    /// A status buffer not in flight: the tag's own with streams, the
    /// lowest free without.
    fn free_status(&self, tag: u16) -> Option<u16> {
        if self.streams {
            return (self.statuses & (1 << (tag - 1)) == 0).then_some(tag);
        }
        let free = (!self.statuses).trailing_zeros() as usize;
        (free < MAX_TAGS).then_some(free as u16 + 1)
    }

    fn start_status(&mut self, n: u16) {
        self.statuses |= 1 << (n - 1);
        self.push(Action::Start(Pipe::Status, n));
    }

    fn start_data(&mut self, tag: u16, into_host: bool) {
        if let Some(slot) = self.slot(tag) {
            slot.data_running = true;
        }
        self.push(Action::Start(Pipe::data(into_host), tag));
    }

    /// The next transfer to start or stop.
    pub fn next_action(&mut self) -> Option<Action> {
        self.actions.pop()
    }

    /// The transfer started as `Action::Start(pipe, n)` ended with
    /// `result`, the bytes that moved; a status's are `status`.
    pub fn ended(&mut self, pipe: Pipe, n: u16, result: Result<usize, PipeError>, status: &[u8]) {
        if result == Err(PipeError::Gone) {
            self.gone();
            return;
        }
        match pipe {
            Pipe::Command => {
                let Some(slot) = self.slot(n) else { return };
                slot.command_running = false;
                if result.is_err() {
                    self.break_with(Broken::RefusedCommand);
                }
            }
            Pipe::Status => {
                if n == 0 || usize::from(n) > MAX_TAGS {
                    return;
                }
                self.statuses &= !(1 << (n - 1));
                match result {
                    Ok(came) => self.status(n, &status[..came.min(status.len())]),
                    Err(_) => self.break_with(Broken::StatusPipe),
                }
            }
            Pipe::DataIn | Pipe::DataOut => {
                let Some(slot) = self.slot(n) else { return };
                if !slot.data_running {
                    return;
                }
                slot.data_running = false;
                match result {
                    Ok(moved) | Err(PipeError::Stall(moved)) if slot.data_cancelled => {
                        // Stopped, as asked, after this much moved.
                        slot.moved = moved as u32;
                    }
                    Ok(moved) => slot.moved = moved as u32,
                    Err(_) if slot.data_cancelled => {}
                    Err(_) => self.break_with(Broken::DataPipe),
                }
            }
        }
    }

    /// An IU came on the status pipe, in status buffer `n`.
    fn status(&mut self, n: u16, bytes: &[u8]) {
        if bytes.len() < 4 {
            self.break_with(Broken::ShortStatus);
            return;
        }
        let tag = u16::from_be_bytes([bytes[2], bytes[3]]);
        if self.streams && tag != n {
            self.break_with(Broken::OtherStream);
            return;
        }
        let streams = self.streams;
        let Some(slot) = self.slot(tag) else {
            // Linux leaves an IU for no command alone. Its buffer goes
            // again, without streams, so that each command waiting still
            // has one.
            if !streams && self.slots.iter().any(|s| s.used && s.ending == Ending::Pending) {
                self.start_status(n);
            }
            return;
        };
        if slot.ending != Ending::Pending {
            // Its status came already: a second one is not believed.
            return;
        }
        match bytes[0] {
            iu::SENSE if bytes.len() >= 16 => {
                // As much sense data as it says, and as came.
                let length = usize::from(u16::from_be_bytes([bytes[14], bytes[15]]));
                let sense = &bytes[16..bytes.len().min(16 + length)];
                slot.ending = match bytes[6] {
                    status::GOOD | status::CONDITION_MET => Ending::Good,
                    status::CHECK_CONDITION => Ending::Failed(Sense::parse(sense)),
                    // Busy, a full task set, a reservation: the command
                    // goes again.
                    _ => Ending::Failed(None),
                };
                // A command that failed leaves its data waiting; it is
                // stopped, as Linux unlinks it.
                if slot.ending != Ending::Good && slot.data_running {
                    self.cancel_data(tag);
                }
            }
            iu::RESPONSE if bytes.len() >= 8 => {
                // To a command, a Response IU says it was refused: Linux
                // takes a wrong logical unit as one that is not there, and
                // anything else as the device's error.
                if bytes[7] != response::INCORRECT_LUN {
                    self.break_with(Broken::RefusedIu);
                    return;
                }
                slot.ending = Ending::Failed(None);
                if slot.data_running {
                    self.cancel_data(tag);
                }
            }
            iu::READ_READY | iu::WRITE_READY => {
                let into_host = bytes[0] == iu::READ_READY;
                let expected = slot.length > 0
                    && slot.into_host == into_host
                    && !slot.data_running
                    && slot.moved == 0;
                if streams || !expected {
                    // With streams the data waits already; Linux leaves
                    // such an IU alone. The status it took the place of
                    // goes again.
                    self.start_status(n);
                    return;
                }
                self.start_data(tag, into_host);
                // Then its status: in the buffer this IU came in, free.
                self.start_status(n);
            }
            _ => self.break_with(Broken::BadStatus),
        }
    }

    /// Stops the data transfer of the command with tag `tag`.
    fn cancel_data(&mut self, tag: u16) {
        let Some(slot) = self.slot(tag) else { return };
        slot.data_cancelled = true;
        let pipe = Pipe::data(slot.into_host);
        self.push(Action::Cancel(pipe, tag));
    }

    /// The next command that finished: its tag and how it ended. One whose
    /// status came finishes once its transfers have ended.
    pub fn next_done(&mut self) -> Option<(u16, Result<Reply, TransportError>)> {
        let at = self.slots.iter().position(|s| {
            s.used && s.ending != Ending::Pending && !s.command_running && !s.data_running
        })?;
        let slot = core::mem::replace(&mut self.slots[at], Slot::FREE);
        let reply = match slot.ending {
            // What moved is known only now, when its data has ended.
            Ending::Good => Ok(Reply::Good { residue: slot.length.saturating_sub(slot.moved) }),
            Ending::Failed(sense) => Ok(Reply::Failed(sense)),
            Ending::Reset(why) => Err(TransportError::Failed(why.why())),
            Ending::Gone | Ending::Pending => Err(TransportError::Gone),
        };
        Some((at as u16 + 1, reply))
    }

    /// A command went unanswered too long: the device needs a reset.
    pub fn timed_out(&mut self) {
        if self.in_flight() > 0 {
            self.break_with(Broken::TimedOut);
        }
    }

    /// The device was reset after it broke ([`Uas::broken`]), with
    /// `result`: every transfer in flight is forgotten, and every command
    /// in flight ends, to go again, or fails if the device is gone.
    pub fn reset(&mut self, result: Result<(), PipeError>) {
        let why = self.broken.take().unwrap_or(Broken::TimedOut);
        self.actions.clear();
        self.statuses = 0;
        if result == Err(PipeError::Gone) {
            self.gone();
            return;
        }
        for slot in self.slots.iter_mut().filter(|s| s.used) {
            slot.command_running = false;
            slot.data_running = false;
            slot.ending = Ending::Reset(why);
        }
        if result.is_err() {
            // It could not be reset: once more, at the next command.
            self.broken = Some(Broken::ResetFailed);
        }
    }

    /// The device is gone: every command in flight fails, and none starts
    /// again.
    pub fn gone(&mut self) {
        self.gone = true;
        self.broken = None;
        self.actions.clear();
        self.statuses = 0;
        for slot in self.slots.iter_mut().filter(|s| s.used) {
            slot.command_running = false;
            slot.data_running = false;
            slot.ending = Ending::Gone;
        }
    }
}

/// What carries a UAS interface's transfers for [`OneAtATime`].
pub trait Pipes {
    /// Starts a transfer, as [`Action::Start`] names it: the Command IU
    /// `iu`, a status into buffer `n`, or `data`.
    fn start(
        &mut self,
        pipe: Pipe,
        n: u16,
        iu: &[u8],
        data: &mut Data<'_>,
    ) -> Result<(), PipeError>;

    /// Stops the transfer started as `pipe` and `n`; it still ends.
    fn cancel(&mut self, pipe: Pipe, n: u16);

    /// The next transfer that ended, waiting at most `ms` for one: its
    /// pipe, its number, and the bytes that moved, a status's copied into
    /// `status` and data that came into `data`.
    fn wait(
        &mut self,
        ms: u32,
        status: &mut [u8; STATUS_LENGTH],
        data: &mut Data<'_>,
    ) -> Option<(Pipe, u16, Result<usize, PipeError>)>;

    /// Resets the device: its port, then its endpoints and streams. Every
    /// transfer in flight is gone with them, and does not end.
    fn reset(&mut self) -> Result<(), PipeError>;

    /// The time, in milliseconds from any start.
    fn now_ms(&mut self) -> u64;
}

impl<P: Pipes + ?Sized> Pipes for &mut P {
    fn start(
        &mut self,
        pipe: Pipe,
        n: u16,
        iu: &[u8],
        data: &mut Data<'_>,
    ) -> Result<(), PipeError> {
        (**self).start(pipe, n, iu, data)
    }

    fn cancel(&mut self, pipe: Pipe, n: u16) {
        (**self).cancel(pipe, n);
    }

    fn wait(
        &mut self,
        ms: u32,
        status: &mut [u8; STATUS_LENGTH],
        data: &mut Data<'_>,
    ) -> Option<(Pipe, u16, Result<usize, PipeError>)> {
        (**self).wait(ms, status, data)
    }

    fn reset(&mut self) -> Result<(), PipeError> {
        (**self).reset()
    }

    fn now_ms(&mut self) -> u64 {
        (**self).now_ms()
    }
}

/// A UAS interface's commands one at a time over `pipes`, as a
/// [`Transport`]; `uas` has no other command in flight.
pub struct OneAtATime<'a, P> {
    /// The interface's commands.
    pub uas: &'a mut Uas,
    /// What carries them.
    pub pipes: P,
}

impl<P: Pipes> OneAtATime<'_, P> {
    /// Carries out what the commands wait for, the command's IU being
    /// `iu`, and resets the device if they broke it.
    fn act(&mut self, iu: &[u8; COMMAND_LENGTH], data: &mut Data<'_>) {
        loop {
            while let Some(action) = self.uas.next_action() {
                match action {
                    Action::Start(pipe, n) => {
                        if let Err(err) = self.pipes.start(pipe, n, iu, data) {
                            self.uas.ended(pipe, n, Err(err), &[]);
                        }
                    }
                    Action::Cancel(pipe, n) => self.pipes.cancel(pipe, n),
                }
            }
            if self.uas.broken().is_none() {
                return;
            }
            let result = self.pipes.reset();
            self.uas.reset(result);
            if result.is_err() {
                return;
            }
        }
    }
}

impl<P: Pipes> Transport for OneAtATime<'_, P> {
    fn command(
        &mut self,
        lun: u8,
        cdb: &Cdb,
        mut data: Data<'_>,
        timeout_ms: u32,
    ) -> Result<Reply, TransportError> {
        if self.uas.is_gone() {
            return Err(TransportError::Gone);
        }
        let mut status = [0u8; STATUS_LENGTH];
        let length = data.len() as u32;
        let way = (length > 0).then(|| (data.into_host(), length));
        // A device its last reset failed is reset first.
        let mut iu = [0u8; COMMAND_LENGTH];
        self.act(&iu, &mut data);
        let failed = TransportError::Failed("it could not be reset");
        let tag = self.uas.free_tag().ok_or(failed)?;
        iu = self.uas.start(tag, lun, cdb, way).ok_or(failed)?;
        let deadline = self.pipes.now_ms().saturating_add(u64::from(timeout_ms));
        loop {
            self.act(&iu, &mut data);
            while let Some((done, reply)) = self.uas.next_done() {
                if done == tag {
                    return reply;
                }
            }
            if self.uas.is_gone() {
                return Err(TransportError::Gone);
            }
            let left = deadline.saturating_sub(self.pipes.now_ms());
            if left == 0 {
                self.uas.timed_out();
                continue;
            }
            let ms = left.min(u64::from(u32::MAX)) as u32;
            if let Some((pipe, n, result)) = self.pipes.wait(ms, &mut status, &mut data) {
                self.uas.ended(pipe, n, result, &status);
            }
        }
    }
}
