// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `/dev/fb0` (Phase 9, radix-framebuffer(2)): the screen, for consoled(8)
//! to draw its virtual terminals on.
//!
//! The pixels belong to the kernel's framebuffer console (`console`),
//! which draws the log on them. A description that takes the screen
//! (TAKE) is its only writer until it gives it back or goes; the log is
//! kept off the screen meanwhile, and drawn again after. Writes copy the
//! program's bytes in steps of [`STEP`] through a buffer of the kernel's,
//! as the screen's lock masks interrupts and copying from a program may
//! wait for a page.

use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicUsize, Ordering};

use radix_abi::Status;
use radix_abi::errors::Kernel;
use radix_abi::framebuffer::{FramebufferInfo, GIVE_BACK, TAKE};
use radix_klog::info;
use radix_mm::{Flags, HEAP};
use radix_vfs::{In, Kind, Node, NodeRef, Out, Stat};

use super::FileObject;
use crate::console;

/// Bytes copied at a time.
const STEP: usize = 16 * 1024;

/// The description holding the screen, by its address, or 0.
static HOLDER: AtomicUsize = AtomicUsize::new(0);

/// The node of `/dev/fb0`, numbered as its id.
pub struct Framebuffer(pub u64);

impl Node for Framebuffer {
    fn stat(&self) -> Stat {
        let size = console::screen().map_or(0, |(_, height, pitch, _)| (height * pitch) as u64);
        Stat {
            kind: Kind::CharDevice,
            size,
            allocated: 0,
            id: self.0,
            created: 0,
            modified: 0,
            accessed: 0,
        }
    }

    fn kind(&self) -> Kind {
        Kind::CharDevice
    }

    // Reads find nothing: video memory is slow to read. The descriptions'
    // writes are `write` below.
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

/// Whether `node` is `/dev/fb0`.
pub fn is_framebuffer(node: &NodeRef) -> bool {
    radix_vfs::downcast::<Framebuffer>(&**node).is_some()
}

/// The FramebufferInfo of the screen, as bytes.
pub fn info_bytes() -> Result<[u8; FramebufferInfo::SIZE], Status> {
    let (width, height, pitch, format) = console::screen().ok_or(Kernel::NotSupported.status(0))?;
    let mut bytes = [0u8; FramebufferInfo::SIZE];
    bytes[FramebufferInfo::OFFSET_WIDTH..][..4].copy_from_slice(&(width as u32).to_le_bytes());
    bytes[FramebufferInfo::OFFSET_HEIGHT..][..4].copy_from_slice(&(height as u32).to_le_bytes());
    bytes[FramebufferInfo::OFFSET_PITCH..][..4].copy_from_slice(&(pitch as u32).to_le_bytes());
    let fields = [
        (FramebufferInfo::OFFSET_BYTES_PER_PIXEL, format.bytes_per_pixel),
        (FramebufferInfo::OFFSET_RED_SHIFT, format.red.shift),
        (FramebufferInfo::OFFSET_RED_BITS, format.red.bits),
        (FramebufferInfo::OFFSET_GREEN_SHIFT, format.green.shift),
        (FramebufferInfo::OFFSET_GREEN_BITS, format.green.bits),
        (FramebufferInfo::OFFSET_BLUE_SHIFT, format.blue.shift),
        (FramebufferInfo::OFFSET_BLUE_BITS, format.blue.bits),
    ];
    for (at, value) in fields {
        bytes[at] = value;
    }
    Ok(bytes)
}

/// TAKE and GIVE_BACK, for `file`; `may_write` says whether its capability
/// has the write right, which TAKE needs.
pub fn control(file: &FileObject, request: u32, may_write: bool) -> Result<(), Status> {
    let me = file.address();
    match request {
        TAKE if !may_write => Err(Kernel::MissingRights.status(0)),
        TAKE => {
            console::screen().ok_or(Kernel::NotSupported.status(0))?;
            match HOLDER.compare_exchange(0, me, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    info!("Framebuffer: a program took the screen; the log stays off it");
                    console::hand_over();
                    Ok(())
                }
                Err(holder) if holder == me => Ok(()),
                Err(_) => Err(Kernel::Busy.status(0)),
            }
        }
        GIVE_BACK => {
            closed(me);
            Ok(())
        }
        _ => Err(Kernel::NotTerminal.status(0)),
    }
}

/// The description at `address` has gone, or gives the screen back.
pub(super) fn closed(address: usize) {
    if HOLDER.compare_exchange(address, 0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        console::give_back();
        info!("Framebuffer: the screen is the log's again");
    }
}

/// Writes `data` at byte `offset` of the screen, for the description that
/// holds it. Returns how many bytes it wrote.
pub fn write(file: &FileObject, offset: u64, data: &dyn In) -> Result<usize, Status> {
    if HOLDER.load(Ordering::Acquire) != file.address() {
        return Err(Kernel::Busy.status(0));
    }
    let len = data.len();
    let offset = usize::try_from(offset).map_err(|_| Kernel::InvalidArgument.status(1))?;
    let mut buffer: Vec<u8, _> = Vec::new_in(HEAP.with(Flags::MAY_SLEEP));
    buffer.try_reserve_exact(len.min(STEP)).map_err(|_| Kernel::NoMemory.status(0))?;
    while buffer.len() < len.min(STEP) {
        // Room was reserved above.
        let _ = buffer.push_within_capacity(0);
    }
    let mut done = 0;
    while done < len {
        let step = (len - done).min(STEP);
        data.get(done, &mut buffer[..step]).map_err(super::status)?;
        if !console::put_pixels(offset + done, &buffer[..step]) {
            if done == 0 {
                return Err(Kernel::InvalidArgument.status(1));
            }
            break;
        }
        done += step;
    }
    Ok(done)
}
