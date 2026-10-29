// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `/devices/mouse` (Phase 11a, ADR 0148, radix-input(2)): what the
//! machine's pointers do, as Motion records.
//!
//! USB mice and tablets hand each report's motion to [`motion`] from
//! their controller's thread, which puts it into the ring (`events`) and
//! wakes the readers and `poll` itself: no interrupt handler pushes here.

use core::any::Any;

use radix_abi::Status;
use radix_abi::input::{MOTION_ABSOLUTE, MOTIONS_KEPT, Motion as Wire};
use radix_vfs::{In, Kind, Node, NodeRef, Out, Stat};

use super::FileObject;
use super::events::{Events, Record};
use crate::sched::{self, wait};
use crate::user::posix::poll::{self, Polled};

/// Motions kept.
const KEPT: usize = MOTIONS_KEPT as usize;

/// What one report of a pointer changed (radix-input(2)'s Motion).
#[derive(Clone, Copy, Default)]
pub struct Motion {
    /// How far it moved: right and down are more.
    pub dx: i32,
    /// Down.
    pub dy: i32,
    /// The wheels' detents: away from the user, and right, are more.
    pub wheel: i32,
    /// The horizontal one.
    pub pan: i32,
    /// Where it points, from 0 to POSITION_MAX, if it says.
    pub position: Option<(u32, u32)>,
    /// The buttons down after it: BUTTON_* bits.
    pub buttons: u32,
}

/// A motion and when it came.
#[derive(Clone, Copy)]
struct Timed(u64, Motion);

impl Record for Timed {
    const SIZE: usize = Wire::SIZE;

    fn encode(&self, out: &mut [u8]) {
        let Timed(time_ns, m) = *self;
        let (x, y) = m.position.unwrap_or((0, 0));
        let flags = if m.position.is_some() { MOTION_ABSOLUTE } else { 0 };
        out.fill(0);
        out[Wire::OFFSET_TIME_NS..][..8].copy_from_slice(&time_ns.to_le_bytes());
        for (at, value) in [
            (Wire::OFFSET_DX, m.dx as u32),
            (Wire::OFFSET_DY, m.dy as u32),
            (Wire::OFFSET_WHEEL, m.wheel as u32),
            (Wire::OFFSET_PAN, m.pan as u32),
            (Wire::OFFSET_X, x),
            (Wire::OFFSET_Y, y),
            (Wire::OFFSET_BUTTONS, m.buttons),
            (Wire::OFFSET_FLAGS, flags),
        ] {
            out[at..][..4].copy_from_slice(&value.to_le_bytes());
        }
    }
}

static EVENTS: Events<Timed, KEPT> =
    Events::new(Timed(0, Motion { dx: 0, dy: 0, wheel: 0, pan: 0, position: None, buttons: 0 }));
/// The pollers of `/devices/mouse`.
pub static POLLED: Polled = Polled::new();

/// The node of `/devices/mouse`, numbered as its id.
pub struct Mouse(pub u64);

impl Node for Mouse {
    fn stat(&self) -> Stat {
        Stat {
            kind: Kind::CharDevice,
            size: 0,
            allocated: 0,
            id: self.0,
            created: 0,
            modified: 0,
            accessed: 0,
            changed: 0,
            links: 1,
            device: 0,
        }
    }

    fn kind(&self) -> Kind {
        Kind::CharDevice
    }

    // The descriptions' reads are `read` below; the node itself holds
    // nothing, and takes nothing.
    fn read(&self, _offset: u64, _out: &mut dyn Out) -> radix_vfs::Result<usize> {
        Ok(0)
    }

    fn write(&self, _offset: u64, _data: &dyn In) -> radix_vfs::Result<usize> {
        Err(radix_vfs::Error::NotSupported)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Whether `node` is `/devices/mouse`.
pub fn is_mouse(node: &NodeRef) -> bool {
    radix_vfs::downcast::<Mouse>(&**node).is_some()
}

/// The offset a new description starts with: the next motion's.
pub(super) fn opened() -> u64 {
    EVENTS.opened()
}

/// What readers wait on.
fn key() -> usize {
    core::ptr::addr_of!(EVENTS).addr()
}

/// A pointer's report changed `motion`: from its controller's thread.
pub fn motion(motion: Motion) {
    EVENTS.push(Timed(sched::now_ns(), motion));
    wait::wake_all(key());
    POLLED.changed();
}

/// Reads as many of the reader's next motions as `out` holds whole, as
/// `/devices/kbd` reads keys (`keyboard::read`).
pub fn read(file: &FileObject, out: &mut dyn Out, non_blocking: bool) -> Result<usize, Status> {
    EVENTS.read(file, out, non_blocking, key(), "mouse")
}

/// What `poll` finds on `file`: readable while a motion waits for it.
pub fn events(file: &FileObject) -> u16 {
    if EVENTS.readable(file) { poll::READABLE } else { 0 }
}
