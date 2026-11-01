// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The unpacker's own process (ADR 0222). A `.dkv` file comes from
//! anywhere, and reading a zip is where unpacking is attacked, so the
//! archive is read, checked and unpacked by a process that holds nothing
//! else: the archive, to read; the new folder, to fill; and a pipe, for
//! its report. It has no root, so no path it is handed can lead out of
//! the folder, and no other file, socket or process is reachable from it,
//! as OpenSSH's privilege separation keeps parsing away from authority.
//!
//! `bundle unpack` starts it as `bundle unpack --confined`, with the
//! archive as descriptor 0, the pipe as 1 and 2, and the folder as 3. It
//! reports one line: `ok`, the key, the ID, the version, the files and
//! the bytes, tab-separated; or `error` and why.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use super::minisign::PublicKey;
use super::signed::{self, Checked};

/// What the unpacker reports on success.
#[derive(Debug, PartialEq, Eq)]
pub struct Report {
    pub key: PublicKey,
    pub id: String,
    pub version: String,
    pub files: usize,
    pub bytes: u64,
}

impl Report {
    fn of(checked: &Checked) -> Report {
        Report {
            key: checked.key,
            id: checked.manifest.id.0.clone(),
            version: checked.manifest.version.0.clone(),
            files: checked.files,
            bytes: checked.bytes,
        }
    }

    fn line(&self) -> String {
        format!(
            "ok\t{}\t{}\t{}\t{}\t{}\n",
            self.key.encoded(),
            self.id,
            self.version,
            self.files,
            self.bytes
        )
    }

    /// Reads the unpacker's report, which says what it did or why not.
    #[cfg_attr(not(target_os = "radix"), allow(dead_code))]
    pub fn read(text: &str) -> Result<Report, String> {
        let line = text.strip_suffix('\n').unwrap_or(text);
        if let Some(why) = line.strip_prefix("error\t") {
            return Err(why.chars().filter(|c| !c.is_control()).collect());
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let bad = || "the unpacker gave no report".to_string();
        let [ok, key, id, version, files, bytes] = fields.as_slice() else { return Err(bad()) };
        if *ok != "ok" || line.contains('\n') {
            return Err(bad());
        }
        let checked_id = manifest::bundle::check_id(id).is_ok();
        let plain = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_graphic());
        if !checked_id || !plain(version) {
            return Err(bad());
        }
        Ok(Report {
            key: PublicKey::decode(key).map_err(|_| bad())?,
            id: (*id).into(),
            version: (*version).into(),
            files: files.parse().map_err(|_| bad())?,
            bytes: bytes.parse().map_err(|_| bad())?,
        })
    }
}

/// `bundle unpack --confined`: the unpacker itself, in its own process.
pub fn confined() -> i32 {
    // SAFETY: descriptor 1 is the report pipe the starter gave.
    let mut report = unsafe { <File as std::os::fd::FromRawFd>::from_raw_fd(1) };
    let line = match run_confined() {
        Ok(checked) => Report::of(&checked).line(),
        Err(why) => format!("error\t{}\n", why.replace(['\n', '\t'], " ")),
    };
    let ok = line.starts_with("ok");
    let _ = report.write_all(line.as_bytes());
    if ok { 0 } else { 1 }
}

#[cfg(target_os = "radix")]
fn run_confined() -> Result<Checked, String> {
    use std::os::fd::FromRawFd;

    // SAFETY: descriptor 0 is the archive the starter gave.
    let archive = unsafe { File::from_raw_fd(0) };
    let mut target = radix::CapabilityTarget::new(&archive, 3)?;
    signed::unpack(&archive, Some(&mut target))
}

#[cfg(not(target_os = "radix"))]
fn run_confined() -> Result<Checked, String> {
    Err("the unpacker runs on Dukovo".into())
}

/// Unpacks the archive `archive` into the new folder `folder`, in a process
/// of its own; removes the folder if it fails.
pub fn unpack(archive: &File, folder: &Path) -> Result<Report, String> {
    std::fs::create_dir(folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    let result = start(archive, folder);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(folder);
    }
    result
}

#[cfg(not(target_os = "radix"))]
fn start(archive: &File, folder: &Path) -> Result<Report, String> {
    // Built for another system, for tests: the same work, in this process.
    let checked = signed::unpack(archive, Some(&mut signed::FolderTarget(folder)))?;
    Ok(Report::of(&checked))
}

#[cfg(target_os = "radix")]
fn start(archive: &File, folder: &Path) -> Result<Report, String> {
    radix::start(archive, folder)
}

#[cfg(target_os = "radix")]
mod radix {
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use radix_abi::capability::{self, ObjectKind, Rights};
    use radix_abi::directory::{self, OpenFlags};
    use radix_abi::{Status, file, process, startup};

    use super::Report;
    use crate::bundles::signed::{Fill, Target};
    use crate::signin;
    use crate::sys::{self, SYS};

    /// The program the unpacker runs.
    const PROGRAM: &str = "/system/commands/bundle";

    fn status(what: &str) -> impl Fn(Status) -> String + '_ {
        move |status| format!("{what}: {}", sys::status_name(status.0))
    }

    /// Starts the unpacker on `archive`, with `folder`, and reads its report.
    pub fn start(archive: &File, folder: &Path) -> Result<Report, String> {
        // The pipe first: the spare slots below are free descriptors to
        // the C library until they are filled, and pipe2 would take one.
        let mut fds = [0i32; 2];
        // SAFETY: `fds` has room for the two descriptors pipe2 writes.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(format!("a pipe: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: pipe2 made both, and nothing else owns them.
        let (reader, writer) =
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let path =
            std::fs::canonicalize(folder).map_err(|e| format!("{}: {e}", folder.display()))?;
        let path = path.as_os_str().as_bytes();
        let path = path.strip_prefix(b"/").unwrap_or(path);
        let spares = signin::Spares::new(3).map_err(|e| format!("no slots: {e}"))?;
        let [program, child, dir] = [0, 1, 2].map(|i| spares.slot(i));
        let rights = Rights(Rights::READ.bits() | Rights::WRITE.bits());
        directory::open(&SYS, startup::SLOT_ROOT, path, OpenFlags::DIRECTORY, rights, dir, 0)
            .map_err(status("opening the new folder"))?;
        let program_path = PROGRAM.trim_start_matches('/').as_bytes();
        directory::open(
            &SYS,
            startup::SLOT_ROOT,
            program_path,
            OpenFlags::NONE,
            Rights::READ,
            program,
            0,
        )
        .map_err(status("opening the unpacker"))?;
        process::create(&SYS, startup::SLOT_PROCESS, ObjectKind::Process, signin::SLOTS, child)
            .map_err(status("making its process"))?;
        let pipe = sys::slot_of(writer.as_raw_fd());
        let read_only = Rights(Rights::READ.bits() | Rights::COPY.bits());
        let shares = [
            (sys::slot_of(archive.as_raw_fd()), startup::SLOT_FD_BASE, read_only),
            (pipe, startup::SLOT_FD_BASE + 1, Rights::WRITE),
            (pipe, startup::SLOT_FD_BASE + 2, Rights::WRITE),
            (dir, startup::SLOT_FD_BASE + 3, rights),
        ];
        for (from, to, rights) in shares {
            process::share(&SYS, child, from, to, rights, 0)
                .map_err(status("giving the unpacker what it unpacks"))?;
        }
        let block =
            signin::exec_block(PROGRAM.as_bytes(), &[b"bundle", b"unpack", b"--confined"], &[]);
        process::exec(&SYS, child, program, &block).map_err(status("starting the unpacker"))?;
        let number = process::info(&SYS, child).map_err(status("the unpacker's number"))?.number;
        drop(writer);
        drop(spares);
        let mut text = String::new();
        let read = File::from(reader).take(4096).read_to_string(&mut text);
        let mut code = 0;
        // SAFETY: waitpid writes the status.
        unsafe { libc::waitpid(number as i32, &raw mut code, 0) };
        read.map_err(|e| format!("the unpacker's report: {e}"))?;
        let report = Report::read(&text);
        if report.is_ok() && !(libc::WIFEXITED(code) && libc::WEXITSTATUS(code) == 0) {
            return Err("the unpacker did not finish".into());
        }
        if report.is_err() && libc::WIFSIGNALED(code) {
            return Err(format!("the unpacker was ended by signal {}", libc::WTERMSIG(code)));
        }
        report
    }

    /// The folder the unpacker fills, through its capability alone.
    pub struct CapabilityTarget {
        dir: u32,
        /// A slot for each file as it is written: a copy of descriptor 0's,
        /// emptied.
        scratch: u32,
    }

    impl CapabilityTarget {
        pub fn new(archive: &File, fd: i32) -> Result<CapabilityTarget, String> {
            let spare = archive.try_clone().map_err(|e| format!("a slot: {e}"))?;
            let scratch = sys::slot_of(spare.as_raw_fd());
            // The descriptor stays open in name, so nothing else takes its
            // slot; the slot holds each new file in turn.
            std::mem::forget(spare);
            capability::delete(&SYS, scratch).map_err(status("a slot"))?;
            Ok(CapabilityTarget { dir: sys::slot_of(fd), scratch })
        }
    }

    impl Target for CapabilityTarget {
        fn make_folder(&mut self, path: &str) -> Result<(), String> {
            directory::make_directory(&SYS, self.dir, path.as_bytes(), 0o755).map_err(status(path))
        }

        fn write_file(
            &mut self,
            path: &str,
            executable: bool,
            fill: Fill<'_>,
        ) -> Result<(), String> {
            let flags = OpenFlags(
                OpenFlags::CREATE.bits()
                    | OpenFlags::EXCLUSIVE.bits()
                    | OpenFlags::NO_FOLLOW.bits(),
            );
            let mode = if executable { 0o755 } else { 0o644 };
            let rights = Rights(Rights::READ.bits() | Rights::WRITE.bits());
            directory::open(&SYS, self.dir, path.as_bytes(), flags, rights, self.scratch, mode)
                .map_err(status(path))?;
            let slot = self.scratch;
            let mut offset = 0u64;
            let result = fill(&mut |mut bytes| {
                while !bytes.is_empty() {
                    let written =
                        file::write(&SYS, slot, offset, bytes).map_err(status(path))?.count;
                    if written == 0 {
                        return Err(format!("{path}: the disk took nothing"));
                    }
                    offset += written;
                    bytes = &bytes[written as usize..];
                }
                Ok(())
            });
            let _ = capability::delete(&SYS, slot);
            result
        }

        fn free(&self) -> Option<u64> {
            let info = directory::info(&SYS, self.dir).ok()?;
            Some(info.free.saturating_mul(u64::from(info.block_size)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_is_one_strict_line() {
        let mut random = [5u8; 40];
        random[0] = 6;
        let key = super::super::minisign::SecretKey::generate(&random).unwrap().public();
        let report = Report {
            key,
            id: "org.example.hello".into(),
            version: "1.0.0".into(),
            files: 6,
            bytes: 1234,
        };
        assert_eq!(Report::read(&report.line()).unwrap(), report);
        assert_eq!(Report::read("error\tit is not signed\n").unwrap_err(), "it is not signed");
        assert_eq!(Report::read("error\tbad \u{1b}[2J\n").unwrap_err(), "bad [2J");
        let line = report.line();
        for bad in [
            "",
            "ok\n",
            &line.replace("org.example.hello", "Org Example"),
            &line.replace("\t6\t", "\tsix\t"),
            &line.replace("ok", "OK"),
        ] {
            assert!(Report::read(bad).is_err(), "{bad:?}");
        }
    }
}
