// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `/dev`: the devices.
//!
//! One tree of device nodes, which every `dev` mount shows: `null`,
//! `zero`, `random` and `urandom` (both the kernel's generator, which
//! never blocks once seeded at boot), and a block device for each disk
//! and partition `disk` finds. Its tree is fixed, so nothing is made or
//! removed in it; its devices are read and written.

use radix_block::DeviceRef;
use radix_klog::{error, warn};
use radix_sync::Once;
use radix_vfs::synthetic::{BlockNode, FixedDir, Null, Random, SyntheticFs, Zero};
use radix_vfs::{FsKind, FsRef, KArc, NodeRef, Result, mem, next_id};

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
        Ok(root)
    })();
    match made {
        Ok(root) => {
            let _ = ROOT.set(root);
        }
        Err(err) => error!("Files: could not make /dev: {err}"),
    }
}

/// Adds the block device `device` as `name`.
pub fn add_block(name: &str, device: DeviceRef) {
    let Some(root) = ROOT.get() else { return };
    if let Err(err) = BlockNode::make(next_id(), device).and_then(|node| root.add(name, node)) {
        warn!("Files: could not add /dev/{name}: {err}");
    }
}

/// The tree as a filesystem, for a `dev` mount.
pub fn make() -> Result<FsRef> {
    let root: NodeRef = ROOT.get().ok_or(radix_vfs::Error::NoMemory)?.clone();
    SyntheticFs::make(FsKind::Dev, root)
}
