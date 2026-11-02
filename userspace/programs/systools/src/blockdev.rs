// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What rdisk(8) and the mkfs tools share: opening a disk, a partition or
//! an image file as the disk engine's [`disk::Device`], what uses one
//! (radix-block(2)'s GET_IN_USE), and telling the kernel that a disk's
//! partition table changed (REREAD_PARTITIONS, ADR 0301).

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

use radix_abi::{Status, block, file};

use crate::sys::{self, SYS};
use crate::util::{self, Fail};
use crate::{df, proc};

/// A disk, partition or image file, opened.
pub struct Opened {
    /// What it was called, as the messages say it.
    pub shown: String,
    /// For a disk or partition, its name in `/devices`: `vda`, `sdb1`.
    pub name: Option<String>,
    /// Its size in bytes.
    pub size: u64,
    /// Its logical sector size.
    pub sector: u32,
    /// Whether it was opened to write.
    pub writable: bool,
    file: File,
}

impl Opened {
    /// The engine's view of it, whole.
    ///
    /// # Errors
    ///
    /// If its descriptor cannot be copied.
    pub fn device(&self) -> io::Result<disk::Device> {
        Ok(disk::Device::whole(self.file.try_clone()?, self.size, self.sector))
    }

    /// Whether it is a disk or partition, not a file.
    pub fn is_device(&self) -> bool {
        self.name.is_some()
    }

    /// What uses it: GET_IN_USE's flags, 0 for a file.
    pub fn uses(&self) -> u32 {
        if !self.is_device() {
            return 0;
        }
        let mut flags = [0u8; 4];
        control(&self.file, block::GET_IN_USE, &mut flags).map_or(0, |()| u32::from_le_bytes(flags))
    }

    /// Fails, saying who, if anything uses it.
    ///
    /// # Errors
    ///
    /// If something does.
    pub fn refuse_in_use(&self, doing: &str) -> Result<(), Fail> {
        let uses = self.uses();
        if uses == 0 {
            return Ok(());
        }
        let name = self.name.as_deref().unwrap_or(&self.shown);
        Err(util::fail(format!("{}; {doing} it would lose what is on it", in_use(name, uses))))
    }

    /// Tells the kernel the disk's partition table changed.
    ///
    /// # Errors
    ///
    /// The kernel's status, if it could not read the table again.
    pub fn reread(&self) -> Result<(), Status> {
        control(&self.file, block::REREAD_PARTITIONS, &mut [0u8; 0])
    }
}

/// What uses the device `name`, in words, from GET_IN_USE's `flags`:
/// `vda1 is mounted at /volumes/USB`.
pub fn in_use(name: &str, flags: u32) -> String {
    let mut what = Vec::new();
    if flags & block::IN_USE_MOUNTED != 0 {
        let at: Vec<String> = proc::mounts()
            .unwrap_or_default()
            .iter()
            .filter(|m| m.source == name)
            .map(|m| df::mount_path(m).display().to_string())
            .collect();
        what.push(if at.is_empty() {
            "is mounted".to_string()
        } else {
            format!("is mounted at {}", at.join(" and "))
        });
    }
    if flags & block::IN_USE_SWAP != 0 {
        what.push("is swap that is on (swapoff turns it off)".to_string());
    }
    if flags & block::IN_USE_BTRFS != 0 {
        what.push("is a disk of a Btrfs mounted from another".to_string());
    }
    if flags & block::IN_USE_UNLOCKED != 0 {
        what.push(format!("has its BitLocker volume unlocked as {name}-unlocked"));
    }
    if what.is_empty() {
        what.push("is in use".to_string());
    }
    format!("{name} {}", what.join(", and "))
}

/// The path `operand` names: as given, or a name of `/devices` (`vda`)
/// when no file of that name is here.
pub fn resolve(operand: &OsStr) -> PathBuf {
    let path = Path::new(operand);
    if path.components().count() == 1 && !path.exists() {
        let device = Path::new("/devices").join(path);
        if device.exists() {
            return device;
        }
    }
    path.to_path_buf()
}

/// Opens `operand`, to write if `write`, as a disk or partition, or as an
/// image file of `sector`-byte sectors (512 if not given). A device that
/// refuses writes is opened to read, and says so in `writable`.
///
/// # Errors
///
/// If it cannot be opened, or its size or sector size cannot be read.
pub fn open(operand: &OsStr, write: bool, sector: Option<u32>) -> Result<Opened, Fail> {
    let path = resolve(operand);
    let shown = path.display().to_string();
    let opened = OpenOptions::new().read(true).write(write).open(&path);
    let (file, writable) = match opened {
        Ok(file) => (file, write),
        Err(err) if write && err.raw_os_error() == Some(libc::EROFS) => {
            (File::open(&path).map_err(|e| util::failed(&shown, e))?, false)
        }
        Err(err) => {
            return Err(util::fail(format!("cannot open {shown}: {}", util::strip_os_error(&err))));
        }
    };
    let meta = file.metadata().map_err(|e| util::failed(&shown, e))?;
    if !meta.file_type().is_block_device() {
        if !meta.is_file() {
            return Err(util::fail(format!("{shown} is neither a disk nor a file")));
        }
        let sector = sector.unwrap_or(512);
        if !(512..=4096).contains(&sector) || !sector.is_power_of_two() {
            return Err(util::usage(format!(
                "a sector is 512, 1024, 2048 or 4096 bytes, not {sector}"
            )));
        }
        return Ok(Opened { shown, name: None, size: meta.len(), sector, writable, file });
    }
    let mut bytes = [0u8; 8];
    control(&file, block::GET_SIZE_BYTES, &mut bytes)
        .map_err(|s| util::failed(&shown, sys::io_error(s)))?;
    let mut size = [0u8; 4];
    control(&file, block::GET_SECTOR_SIZE, &mut size)
        .map_err(|s| util::failed(&shown, sys::io_error(s)))?;
    let device_sector = u32::from_le_bytes(size);
    if sector.is_some_and(|asked| asked != device_sector) {
        return Err(util::usage(format!(
            "{shown} has sectors of {device_sector} bytes; --sector-size is for image files"
        )));
    }
    let name = std::fs::canonicalize(&path)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()));
    Ok(Opened {
        shown,
        name,
        size: u64::from_le_bytes(bytes),
        sector: device_sector,
        writable,
        file,
    })
}

/// Block request `request` on `device`, with `value` as its argument.
fn control(device: &File, request: u32, value: &mut [u8]) -> Result<(), Status> {
    let address = value.as_mut_ptr() as u64;
    let slot = sys::slot_of(device.as_raw_fd());
    file::control(&SYS, slot, request, address, address, value.len() as u64).map(drop)
}

/// The partitions of the disk `name` there are now, by number: their
/// names in `/devices`.
pub fn partitions_of(name: &str) -> Vec<(u32, String)> {
    crate::probe::devices()
        .unwrap_or_default()
        .into_iter()
        .filter(|(part, is_part)| *is_part && crate::probe::disk_of(part) == name)
        .filter_map(|(part, _)| {
            let number = crate::probe::block_text(&part, "partition")?.parse().ok()?;
            Some((number, part))
        })
        .collect()
}
