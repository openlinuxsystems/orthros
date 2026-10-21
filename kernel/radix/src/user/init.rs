// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Starting the first program, Spark (spark(8)), at the end of boot.
//!
//! The boot loader loads the boot archive as a module, `bootfs`: a ustar
//! file (`radix-bootfs`) with the programs the system starts with. The
//! kernel checks it whole, finds the first program in it (`init=PATH`,
//! `/system/bin/spark` by default) and starts it in a process of its own
//! (`loader`), with the archive mapped read-only for it, a memory object
//! for the programs it starts in `SLOT_FIRST_EXTRA`, and the words after
//! a lone `--` on the command line as its arguments. From then on Spark
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
use super::process::ProcessObject;
use super::{Result, cap_error, error as status};
use crate::{caps, domains};

/// The word on the boot loader's module line that marks the boot archive.
pub const MODULE_WORD: &str = "bootfs";

/// The first program, unless `init=` names another.
pub const DEFAULT_INIT: &str = "/system/bin/spark";

/// Spark's own memory object, and the one for the programs it starts:
/// 2^order pages each, the largest the kernel makes.
const MEMORY_ORDER: u32 = MAX_ORDER;

/// The most arguments Spark gets.
const ARGS_MAX: usize = 32;

static ARCHIVE: Once<&'static [u8]> = Once::new();

/// Finds the boot archive among the boot modules.
pub fn set_modules(modules: &'static [BootModule]) {
    if let Some(module) = modules.iter().find(|m| m.has_word(MODULE_WORD)) {
        let _ = ARCHIVE.set(module.bytes);
    }
}

/// Whether the boot loader loaded a boot archive.
pub fn has_archive() -> bool {
    ARCHIVE.get().is_some()
}

/// Spark has exited: the system cannot go on without it.
fn spark_exited(process: &ProcessObject, code: u64) {
    panic!("Spark (process {}) exited with code {code:#x}", process.number());
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
    // capability, and the memory for the programs it starts. It is never
    // destroyed.
    let kept = caps::with(|store| store.create_space(2))
        .and_then(|made| made)
        .map_err(cap_error)?;
    let (at, pool) = (SlotRef::new(kept, 0), SlotRef::new(kept, 1));
    caps::create_memory(pool, MEMORY_ORDER).map_err(cap_error)?;
    let program = Program {
        name: "spark",
        image,
        cpu: None,
        memory_order: MEMORY_ORDER,
        role: 0,
        argument: 0,
        args: &words,
        extra: &[(pool, ObjectKind::Memory.rights(), 0)],
        archive: Some(archive),
        domain: domains::root(),
        on_exit: Some(spark_exited),
    };
    let process = loader::start(at, &program)?;
    Ok(process.number())
}
