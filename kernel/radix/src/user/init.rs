// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Starting the first program, Spark (spark(8)), at the end of boot.
//!
//! The boot loader loads the boot archive as a module, `bootfs`: a ustar
//! file (`radix-bootfs`) with the programs the system starts with. The
//! kernel checks it whole, finds the first program in it (`init=PATH`,
//! `/system/commands/spark` by default) and starts it in a process of its
//! own (`loader`), with the archive mapped read-only for it, the root of the
//! file tree in `SLOT_ROOT` (`crate::fs`), from `SLOT_FIRST_EXTRA` on a
//! memory object for the programs it starts, a capability to `services`,
//! the resource domain below the root that its services run in, the
//! power capability (Phase 9), a network capability for all of the
//! network's ports (Phase 12, ADR 0183) and the platform capability
//! (Phase 11, ADR 0208), and the words after a lone `--` on the command
//! line as its arguments. From then on Spark
//! starts every other program. The kernel keeps Spark's process
//! capability, and Spark is the one program that must not end: if it
//! does, the kernel stops.

use alloc::vec::Vec;

use radix_abi::errors::Kernel;
use radix_cap::{ObjectKind, SlotRef};
use radix_hal::BootModule;
use radix_klog::{error, info};
use radix_mm::{Flags, HEAP, MAX_ORDER};
use radix_sync::Once;

use super::loader::{self, Program};
use super::objects::Object;
use super::process::{self, ProcessObject};
use super::{Result, cap_error, error as status};
use crate::domains::{OomClass, Settings};
use crate::{caps, domains};

/// The word on the boot loader's module line that marks the boot archive.
pub const MODULE_WORD: &str = "bootfs";

/// The first program, unless `init=` names another.
pub const DEFAULT_INIT: &str = "/system/commands/spark";

/// Spark's own memory object, and the one for the programs it starts:
/// 2^order pages each, the largest the kernel makes.
const MEMORY_ORDER: u32 = MAX_ORDER;

/// The most arguments Spark gets.
const ARGS_MAX: usize = 32;

/// The domain below the root that Spark makes its services' domains in.
const SERVICES: &str = "services";

static ARCHIVE: Once<&'static [u8]> = Once::new();

/// Finds the boot archive among the boot modules.
pub fn set_modules(modules: &'static [BootModule]) {
    if let Some(module) = modules.iter().find(|m| m.has_word(MODULE_WORD)) {
        let _ = ARCHIVE.set(module.bytes);
    }
}

/// The boot archive's bytes, if the boot loader loaded one.
pub fn archive() -> Option<&'static [u8]> {
    ARCHIVE.get().copied()
}

/// Whether the boot loader loaded a boot archive.
pub fn has_archive() -> bool {
    ARCHIVE.get().is_some()
}

/// Spark has exited: the system cannot go on without it.
fn spark_exited(process: &ProcessObject, code: u64) {
    let signal = process.posix.lock().ended_by;
    match signal {
        Some(signal) => panic!(
            "Spark (process {}) was ended by signal {signal} (exit code {code:#x})",
            process.number()
        ),
        None => panic!("Spark (process {}) exited with code {code:#x}", process.number()),
    }
}

/// Starts `path` from the boot archive with `args` after it, as Spark.
/// Returns whether it started; the log says why not.
pub fn start(path: &str, args: impl Iterator<Item = &'static str>) -> bool {
    let Some(&bytes) = ARCHIVE.get() else {
        error!("Init: the boot loader loaded no `{MODULE_WORD}` module, so nothing starts");
        return false;
    };
    let archive = match radix_bootfs::open(bytes) {
        Ok(archive) => archive,
        Err(err) => {
            error!("Init: the boot archive is damaged: {err}");
            return false;
        }
    };
    let Some(image) = archive.find(path).filter(|e| e.kind == radix_bootfs::Kind::File) else {
        error!("Init: {path} is not in the boot archive");
        return false;
    };
    info!(
        "Init: the boot archive is {} KiB with {} entries; starting {path}",
        bytes.len().div_ceil(1024),
        archive.len()
    );
    match start_spark(path, image.data, bytes, args) {
        Ok(number) => {
            info!("Init: {path} is process {number}");
            true
        }
        Err(err) => {
            error!("Init: could not start {path}: {:#x}", err.0);
            false
        }
    }
}

fn start_spark(
    path: &str,
    image: &[u8],
    archive: &[u8],
    args: impl Iterator<Item = &'static str>,
) -> Result<u64> {
    let mut words = Vec::new_in(HEAP.with(Flags::MAY_SLEEP));
    words.try_reserve(ARGS_MAX + 1).map_err(|_| status(Kernel::NoMemory))?;
    // Room was reserved above.
    let _ = words.push_within_capacity(path);
    for arg in args.take(ARGS_MAX) {
        let _ = words.push_within_capacity(arg);
    }
    // The kernel's own capability space for Spark: its process's original
    // capability, the memory for the programs it starts, the domain its
    // services' domains go below, the machine's power, the network, its
    // platform controls, the names (ADR 0214), and the boot archive's
    // memory and frames. It is never destroyed.
    let slots = loader::archive_slots(archive.len()).saturating_add(7);
    let kept =
        caps::with(|store| store.create_space(slots)).and_then(|made| made).map_err(cap_error)?;
    let slot = |n| SlotRef::new(kept, n);
    let (at, pool, services, power, network, platform) =
        (slot(0), slot(1), slot(2), slot(3), slot(4), slot(5));
    let names = slot(6);
    caps::create_memory(pool, MEMORY_ORDER).map_err(cap_error)?;
    let settings = Settings::new(OomClass::Service);
    caps::create_domain(services, &domains::root(), SERVICES, settings).map_err(cap_error)?;
    process::install(power, ObjectKind::Power, Object::Power)?;
    process::install(platform, ObjectKind::Platform, Object::Platform)?;
    process::install(names, ObjectKind::Names, Object::Names)?;
    caps::with(|store| store.grant_range(network, ObjectKind::Network, 0, 1 << 16))
        .and_then(|done| done)
        .map_err(cap_error)?;
    let program = Program {
        name: "spark",
        image,
        cpu: None,
        memory_order: MEMORY_ORDER,
        role: 0,
        argument: 0,
        args: &words,
        extra: &[
            (pool, ObjectKind::Memory.rights(), 0),
            (services, ObjectKind::ResourceDomain.rights(), 0),
            (power, ObjectKind::Power.rights(), 0),
            (network, ObjectKind::Network.rights(), 0),
            (platform, ObjectKind::Platform.rights(), 0),
            (names, ObjectKind::Names.rights(), 0),
        ],
        archive: Some((archive, slot(7))),
        root: true,
        domain: domains::root(),
        on_exit: Some(spark_exited),
    };
    let process = loader::start(at, &program)?;
    Ok(process.number())
}
