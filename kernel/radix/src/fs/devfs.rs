// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `/devices`: the devices.
//!
//! One tree of device nodes, which every `dev` mount shows: `null`,
//! `zero`, `random` and `urandom` (both the kernel's generator, which
//! never blocks once seeded at boot), a block device for each disk and
//! partition `disk` finds, the terminals (Phase 8, `tty`): `ptmx`, `tty`
//! and `pts/N`, and since Phase 9 the kernel log (`kmsg`), the keyboards
//! (`kbd`, from `keyboard`) and the screen (`fb0`, from `framebuffer`). Programs
//! make and remove nothing in it; only the kernel adds devices, and
//! `pts/N` comes and goes with its pseudo-terminal.

use radix_block::DeviceRef;
use radix_klog::{error, warn};
use radix_sync::Once;
use radix_vfs::synthetic::{BlockNode, FixedDir, Null, Random, SyntheticFs, Zero};
use radix_vfs::{FsKind, FsRef, KArc, Kind, Node, NodeRef, Result, mem, next_id};

static ROOT: Once<KArc<FixedDir>> = Once::new();

/// Makes the tree with its character devices.
pub fn init() {
    let made = (|| -> Result<KArc<FixedDir>> {
        let root = FixedDir::new(next_id())?;
        let null: NodeRef = mem::arc(Null(next_id()))?;
        root.add("null", null)?;
        let zero: NodeRef = mem::arc(Zero(next_id()))?;
        root.add("zero", zero)?;
        for name in ["random", "urandom"] {
            let random: NodeRef =
                mem::arc(Random { id: next_id(), fill: crate::user::random::fill })?;
            root.add(name, random)?;
        }
        let kmsg: NodeRef = mem::arc(super::kmsg::Kmsg(next_id()))?;
        root.add("kmsg", kmsg)?;
        let kbd: NodeRef = mem::arc(super::keyboard::Keyboard(next_id()))?;
        root.add("kbd", kbd)?;
        let fb0: NodeRef = mem::arc(super::framebuffer::Framebuffer(next_id()))?;
        root.add("fb0", fb0)?;
        super::tty::init(&root)?;
        Ok(root)
    })();
    match made {
        Ok(root) => {
            let _ = ROOT.set(root);
        }
        Err(err) => error!("Files: could not make /devices: {err}"),
    }
}

/// The character devices of the tree that a device node made elsewhere
/// opens, by Linux's numbers for them (ADR 0135).
const NUMBERS: [((u32, u32), &str); 8] = [
    ((1, 3), "null"),
    ((1, 5), "zero"),
    ((1, 8), "random"),
    ((1, 9), "urandom"),
    ((1, 11), "kmsg"),
    ((5, 0), "tty"),
    ((5, 2), "ptmx"),
    ((29, 0), "fb0"),
];

/// The device a device node of `kind` that stands for device number
/// `device` opens: its node in the tree, if the tree has it.
pub fn by_number(kind: Kind, device: u64) -> Option<NodeRef> {
    let parts = radix_vfs::device_parts(device);
    let (_, name) = NUMBERS.iter().find(|(number, _)| *number == parts)?;
    if kind != Kind::CharDevice {
        return None;
    }
    ROOT.get()?.lookup(name).ok()
}

/// Adds the block device `device` as `name`.
pub fn add_block(name: &str, device: DeviceRef) {
    let Some(root) = ROOT.get() else { return };
    if let Err(err) = BlockNode::make(next_id(), device).and_then(|node| root.add(name, node)) {
        warn!("Files: could not add /devices/{name}: {err}");
    }
}

/// The tree as a filesystem, for a `dev` mount.
pub fn make() -> Result<FsRef> {
    let root: NodeRef = ROOT.get().ok_or(radix_vfs::Error::NoMemory)?.clone();
    SyntheticFs::make(FsKind::Dev, root)
}
