// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! unlock(8): unlocks a BitLocker volume, Windows' disk encryption, with
//! its password or its recovery key, as cryptsetup's `open --type bitlk`
//! does on Linux (ADR 0240). The key the password or recovery key opens
//! is unwrapped here, and handed to the kernel with radix-block(2)'s
//! `UNLOCK`, which makes the volume unlocked a device of its own,
//! `/devices/<name>-unlocked`, that volumed(8) mounts at `/volumes`.
//!
//! A volume whose BitLocker is suspended keeps its key in the clear, and
//! unlocks without asking.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;

use radix_abi::{Status, block, file};
use radix_bitlocker::format::{self, METADATA_AREA, Metadata, Volume, protection};
use radix_bitlocker::keys::{self, Key, Secret, Zeroizing};

use crate::args::{Arg, Args};
use crate::prompt::{self, Answer};
use crate::sys::{self, SYS};
use crate::util::{self, Fail, usage};

/// How many times a password or recovery key is asked for at a terminal.
const TRIES: usize = 3;

pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let (mut read_only, mut what) = (false, None::<String>);
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('r') => read_only = true,
            Arg::Long(long) if long == "read-only" => read_only = true,
            Arg::Operand(operand) if what.is_none() => {
                what = Some(operand.to_string_lossy().into_owned());
            }
            other => return Err(args.unknown(&other)),
        }
    }
    let Some(what) = what else { return Err(usage("no device given".into())) };
    let name = what.strip_prefix("/devices/").unwrap_or(&what).to_string();
    let path = format!("/devices/{name}");
    let device = open(&path, &mut read_only).map_err(|e| util::failed(&path, e))?;
    let (block, sector) =
        metadata_block(&device).map_err(|why| util::fail(format!("{name}: {why}")))?;
    let metadata =
        Metadata::parse(&block, sector).map_err(|e| util::fail(format!("{name}: {e}")))?;
    let mut out = io::stdout().lock();
    if let Some(description) = metadata.description() {
        let description: String = char::decode_utf16(description)
            .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect();
        writeln!(out, "unlock: {name} is a BitLocker volume: {description}")?;
    }
    drop(out);
    let key = match keys::unlock_clear(&metadata) {
        // BitLocker is suspended.
        Ok(key) => key,
        Err(_) => ask_and_unwrap(&name, &metadata)?,
    };
    let flags = if read_only { block::UNLOCK_READ_ONLY } else { 0 };
    hand_to_kernel(&device, &key, flags).map_err(|status| {
        util::fail(format!("{name}: {}", util::strip_os_error(&sys::io_error(status))))
    })?;
    let ro = if read_only { ", read-only" } else { "" };
    writeln!(io::stdout(), "unlock: {name} is unlocked as /devices/{name}-unlocked{ro}")?;
    Ok(0)
}

/// Opens the device at `path` to write, or to read when it refuses writes
/// or `read_only` asks; sets `read_only` if it is only read.
fn open(path: &str, read_only: &mut bool) -> io::Result<File> {
    if !*read_only {
        match OpenOptions::new().read(true).write(true).open(path) {
            Err(err) if err.raw_os_error() == Some(libc::EROFS) => *read_only = true,
            done => return done,
        }
    }
    File::open(path)
}

/// The first copy of the volume's metadata that reads, and the bytes of
/// its sectors.
fn metadata_block(device: &File) -> Result<(Vec<u8>, u64), String> {
    let mut boot = [0u8; 512];
    device.read_exact_at(&mut boot, 0).map_err(|e| util::strip_os_error(&e))?;
    let (blocks, sector) = match format::volume(&boot) {
        Some(Volume::Locked { metadata, sector }) => (metadata, sector),
        Some(Volume::Vista) => return Err(format::Error::NotSupported.to_string()),
        None => return Err(format::Error::NotBitLocker.to_string()),
    };
    let mut block = vec![0u8; METADATA_AREA as usize];
    for at in blocks {
        if device.read_exact_at(&mut block, at).is_ok() && Metadata::parse(&block, sector).is_ok() {
            return Ok((block, sector));
        }
    }
    Err(format::Error::Damaged.to_string())
}

/// Asks for the volume's password or recovery key, whichever it takes,
/// and unwraps its key with what is typed: up to [`TRIES`] times at a
/// terminal, once from a pipe.
fn ask_and_unwrap(name: &str, metadata: &Metadata<'_>) -> Result<Key, Fail> {
    let has = |wanted| metadata.vmks().any(|v| v.protection == wanted);
    let question = match (has(protection::PASSWORD), has(protection::RECOVERY)) {
        (true, true) => format!("Password or recovery key for {name}: "),
        (true, false) => format!("Password for {name}: "),
        (false, true) => format!("Recovery key for {name} (48 digits): "),
        (false, false) => {
            return Err(util::fail(format!(
                "{name} unlocks only with the TPM or a startup key of the PC that \
                 encrypted it; Windows' recovery key unlocks it here once it has one"
            )));
        }
    };
    let tries = if prompt::interactive() { TRIES } else { 1 };
    for _ in 0..tries {
        let mut line = match prompt::ask(&question, false)? {
            Answer::Line(line) => line,
            Answer::Stopped => return Err(util::fail(format!("{name} is left locked"))),
        };
        let unwrapped = unwrap_with(metadata, &line);
        accounts::wipe(&mut line);
        match unwrapped {
            Ok(key) => return Ok(key),
            Err(format::Error::WrongKey | format::Error::NoProtector) => {
                writeln!(
                    io::stderr(),
                    "unlock: {name}: that password or recovery key does not open it"
                )?;
            }
            Err(err) => return Err(util::fail(format!("{name}: {err}"))),
        }
    }
    Err(util::fail(format!("{name} is left locked")))
}

/// The volume's key, unwrapped with `typed`: a recovery key if it reads
/// as one, else a password.
fn unwrap_with(metadata: &Metadata<'_>, typed: &[u8]) -> Result<Key, format::Error> {
    let typed = std::str::from_utf8(typed).map_err(|_| format::Error::WrongKey)?;
    if let Some(recovery) = keys::recovery_key(typed) {
        match keys::unlock(metadata, Secret::Recovery(&recovery)) {
            Err(format::Error::WrongKey | format::Error::NoProtector) => {}
            done => return done,
        }
    }
    keys::unlock(metadata, Secret::Password(typed))
}

/// radix-block(2)'s `UNLOCK` on `device`, with `key` and `flags`.
fn hand_to_kernel(device: &File, key: &Key, flags: u32) -> Result<(), Status> {
    let mut value = Zeroizing::new([0u8; block::UNLOCK_SIZE as usize]);
    value[..4].copy_from_slice(&u32::from(key.method).to_le_bytes());
    value[4..8].copy_from_slice(&flags.to_le_bytes());
    value[8..8 + key.bytes().len()].copy_from_slice(key.bytes());
    let address = value.as_mut_ptr() as u64;
    let slot = sys::slot_of(device.as_raw_fd());
    file::control(&SYS, slot, block::UNLOCK, address, address, value.len() as u64).map(drop)
}
