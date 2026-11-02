// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `/devices/fuse` (ADR 0226): where a program that serves a filesystem
//! reads the kernel's requests and writes its answers, in Linux's FUSE
//! protocol (`radix_fuse`).
//!
//! Opening it makes a connection: the description's node is a
//! [`Channel`] of its own, which radix-directory(2)'s `mount_fuse` mounts.
//! A read of the description takes a request, waiting for one unless the
//! description is non-blocking; a write gives an answer; a poll finds it
//! readable while a request waits, and when the mount has gone (a read
//! then fails with ENODEV, which tells the program to end). When the
//! program's last description of it goes, the connection is lost, and
//! the mount's calls fail as not connected.
//!
//! The device is Linux's number 10:229, which a device node made
//! elsewhere opens too.

use core::any::Any;

use radix_abi::Status;
use radix_abi::errors::{File as FileError, Kernel};
use radix_fuse::{Caller, Connection, DeviceError, Hooks, Wake};
use radix_vfs::{Access, In, KArc, Kind, Node, NodeRef, Out, Stat, next_id};

use super::FileObject;
use crate::sched::{self, wait};
use crate::user::posix::poll::{self, Polled};
use crate::user::posix::signal::{Restart, interrupted};

/// What polls of every connection look at: a request queued, or a mount
/// gone, wakes them.
pub static POLLED: Polled = Polled::new();

/// Registers the waits, wake-ups, callers and clock the connections use.
pub fn init() {
    radix_fuse::set_hooks(Hooks { wait: wait_for, wake, caller, now: sched::now_ns });
}

fn wait_for(key: usize, ready: &dyn Fn() -> bool, signals: bool) -> Wake {
    let me = sched::running();
    let stopping = || me.as_ref().is_some_and(|me| me.should_stop());
    wait::wait_until(key, "fuse", || ready() || stopping() || signals && sched::interrupted());
    if ready() {
        Wake::Ready
    } else if stopping() {
        Wake::Stop
    } else {
        Wake::Signal
    }
}

fn wake(key: usize) {
    wait::wake_all(key);
    POLLED.changed();
}

fn caller() -> Caller {
    let me = sched::running();
    let Some(user) = me.as_ref().and_then(|me| me.user()) else { return Caller::default() };
    let identity = user.process.identity();
    let pid = u32::try_from(user.process.number()).unwrap_or(u32::MAX);
    Caller { uid: identity.user, gid: identity.group, pid }
}

/// A character device's attributes.
fn char_device(id: u64) -> Stat {
    Stat {
        kind: Kind::CharDevice,
        size: 0,
        allocated: 0,
        id,
        created: 0,
        modified: 0,
        accessed: 0,
        changed: 0,
        links: 1,
        device: 0,
    }
}

/// `/devices/fuse`: opening it makes a connection.
pub struct Fuse(pub u64);

impl Node for Fuse {
    fn stat(&self) -> Stat {
        char_device(self.0)
    }

    fn kind(&self) -> Kind {
        Kind::CharDevice
    }

    // Anyone's, as on Linux: serving a filesystem gives nothing, and
    // mounting one needs the control right.
    fn access(&self) -> Access {
        Access { owner: 0, group: 0, mode: 0o666 }
    }

    fn open(&self) -> radix_vfs::Result<Option<NodeRef>> {
        let channel: NodeRef =
            radix_vfs::mem::arc(Channel { id: next_id(), conn: Connection::new()? })?;
        Ok(Some(channel))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A connection, as its program's descriptions hold it.
pub struct Channel {
    id: u64,
    conn: KArc<Connection>,
}

impl Channel {
    /// Its connection.
    pub fn connection(&self) -> &KArc<Connection> {
        &self.conn
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.conn.lose();
    }
}

impl Node for Channel {
    fn stat(&self) -> Stat {
        char_device(self.id)
    }

    fn kind(&self) -> Kind {
        Kind::CharDevice
    }

    fn access(&self) -> Access {
        Access { owner: 0, group: 0, mode: 0o666 }
    }

    // The kernel's own reads and writes through the node find nothing:
    // the descriptions' are `read` and `write` below.
    fn read(&self, _offset: u64, _out: &mut dyn Out) -> radix_vfs::Result<usize> {
        Ok(0)
    }

    fn write(&self, _offset: u64, data: &dyn In) -> radix_vfs::Result<usize> {
        Ok(data.len())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The connection `node` is, if it is one.
pub fn channel_of(node: &NodeRef) -> Option<&Channel> {
    radix_vfs::downcast::<Channel>(&**node)
}

fn status(err: DeviceError) -> Status {
    match err {
        DeviceError::WouldBlock => Kernel::WouldBlock.status(0),
        DeviceError::Interrupted => interrupted(Restart::Always),
        // ENODEV, as Linux's FUSE: the mount is gone, so the program ends.
        DeviceError::NoDevice => FileError::Unmounted.status(0),
        DeviceError::Invalid => Kernel::InvalidArgument.status(0),
        DeviceError::NotFound => FileError::NotFound.status(0),
        DeviceError::BadAddress => Kernel::BadAddress.status(0),
        DeviceError::NoMemory => Kernel::NoMemory.status(0),
    }
}

/// The program reads a request from `file`'s connection into `out`.
pub fn read(file: &FileObject, out: &mut dyn Out, non_blocking: bool) -> Result<usize, Status> {
    let channel = channel_of(&file.at.node).ok_or(Kernel::InvalidArgument.status(0))?;
    channel.conn.read(out, non_blocking).map_err(status)
}

/// The program writes an answer to `file`'s connection.
pub fn write(file: &FileObject, data: &dyn In) -> Result<usize, Status> {
    let channel = channel_of(&file.at.node).ok_or(Kernel::InvalidArgument.status(0))?;
    channel.conn.write(data).map_err(status)
}

/// What a poll of `file`'s connection finds: readable while a request
/// waits, readable and in error once the mount has gone, and always
/// writable.
pub fn events(file: &FileObject) -> u16 {
    let Some(channel) = channel_of(&file.at.node) else { return poll::ERROR };
    let conn = &channel.conn;
    let mut events = poll::WRITABLE;
    if conn.readable() {
        events |= poll::READABLE;
    }
    if conn.gone() {
        events |= poll::ERROR;
    }
    events
}
