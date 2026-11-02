// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The test of filesystems that programs serve (the `fuse` boot test, ADR
//! 0226). It serves a filesystem in memory with fuser, as a program
//! written for Linux's FUSE does, on a connection to `/devices/fuse` that
//! it mounts at /tmp/fuse-test with radix-directory(2)'s `mount_fuse`,
//! and uses it as any program uses files:
//!
//! - the mount is listed, as a `fuse` filesystem from `fuse-test`;
//! - files are made, written, read, appended to, cut short, renamed and
//!   removed, and a large one goes over in the pieces INIT allows;
//! - directories are made, listed, moved and removed, a full one is not
//!   removed, and symbolic and hard links work;
//! - the owners and modes the program gives are what the kernel checks
//!   (`default_permissions`): a root that acts for another user, as their
//!   shell's does, is refused a private file, and what it makes is
//!   theirs, the program having been told who asked;
//! - a set-ID bit is kept by the program but never shown (`nosuid`), and
//!   a device node on the mount opens nothing (`nodev`);
//! - unmounting ends the program's loop (a read fails with ENODEV), and
//!   the folder is empty again;
//! - a mount whose program has gone fails its calls as not connected,
//!   and is unmounted all the same.
//!
//! Like the C tests, it prints "fuse-test: <part>" for each part that
//! passed, a line for each check that fails, and "fuse-test: every check
//! passed"; it exits with how many checks failed.

mod memfs;

use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use fuser::{Session, SessionACL};
use radix_abi::capability::{self, Rights};
use radix_abi::directory::{self, MountFlags, OpenFlags};
use radix_abi::errors::{File as FileError, Kernel};
use radix_abi::{Status, Syscalls, names, startup};

use crate::memfs::MemFs;

/// The test's name, which every line it prints starts with.
const NAME: &str = "fuse-test";

/// Where the filesystem is mounted.
const DIR: &str = "/tmp/fuse-test";

/// The mount's source, as the mounts list names it.
const SOURCE: &str = "fuse-test";

/// A user other than root, for the permission checks.
const OTHER: u32 = 1000;

/// The slot Spark gives the test the names capability in, as its
/// descriptor 3 (`--names`).
const NAMES: u32 = startup::SLOT_FD_BASE + 3;

/// How many checks have failed.
static FAILURES: AtomicUsize = AtomicUsize::new(0);

/// Says that a check failed, and counts it.
fn failed(file: &str, line: u32, what: &str) {
    println!("{NAME}: FAILED: {file}:{line}: {what}");
    FAILURES.fetch_add(1, Ordering::Relaxed);
}

/// Checks that a condition holds, and says which did not.
macro_rules! check {
    ($condition:expr) => {
        if !$condition {
            failed(file!(), line!(), stringify!($condition));
        }
    };
    ($condition:expr, $($what:tt)+) => {
        if !$condition {
            failed(file!(), line!(), &format!($($what)+));
        }
    };
}

/// The value of a result, or a failed check that names it.
macro_rules! ok {
    ($result:expr) => {
        match $result {
            Ok(value) => Some(value),
            Err(err) => {
                failed(file!(), line!(), &format!("{}: {err:?}", stringify!($result)));
                None
            }
        }
    };
}

/// Runs a part of the test, and says it passed if no check of it failed.
fn part(what: &str, run: impl FnOnce()) {
    let before = FAILURES.load(Ordering::Relaxed);
    run();
    if FAILURES.load(Ordering::Relaxed) == before {
        println!("{NAME}: {what}");
    } else {
        println!("{NAME}: FAILED: {what}");
    }
}

/// The kernel, through the `syscall` instruction.
struct Sys;

impl Syscalls for Sys {
    #[cfg(target_os = "radix")]
    #[inline(always)]
    fn syscall(&self, number: u64, args: [u64; 10]) -> (u64, [u64; 10]) {
        let mut r = args;
        let status: u64;
        // SAFETY: the kernel reads only these registers, writes only these
        // and rcx and r11 (declared clobbered), and touches no memory of
        // the program's but the buffers the arguments name, which the
        // wrappers pass from references they hold; it does not use the
        // program's stack.
        unsafe {
            std::arch::asm!(
                "syscall",
                inlateout("rax") number => status,
                inlateout("rdi") r[0],
                inlateout("rsi") r[1],
                inlateout("rdx") r[2],
                inlateout("r10") r[3],
                inlateout("r8") r[4],
                inlateout("r9") r[5],
                inlateout("r12") r[6],
                inlateout("r13") r[7],
                inlateout("r14") r[8],
                inlateout("r15") r[9],
                out("rcx") _,
                out("r11") _,
                options(nostack),
            );
        }
        (status, r)
    }

    #[cfg(not(target_os = "radix"))]
    fn syscall(&self, _number: u64, args: [u64; 10]) -> (u64, [u64; 10]) {
        (Kernel::NoSuchSyscall.status(0).0, args)
    }
}

/// A status by its error's name.
fn named(status: Status) -> String {
    if let Some(error) = Kernel::from_status(status) {
        error.name().to_string()
    } else if let Some(error) = FileError::from_status(status) {
        format!("file.{}", error.name())
    } else {
        format!("error {:#x}", status.0)
    }
}

/// Mounts the connection `device` holds at [`DIR`].
fn mount(device: &File) -> Result<(), String> {
    let slot = startup::SLOT_FD_BASE + device.as_raw_fd() as u32;
    let path = DIR.trim_start_matches('/').as_bytes();
    directory::mount_fuse(&Sys, startup::SLOT_ROOT, path, slot, SOURCE.as_bytes(), MountFlags::NONE)
        .map_err(named)
}

/// Unmounts [`DIR`].
fn unmount() -> Result<(), String> {
    directory::unmount(&Sys, startup::SLOT_ROOT, DIR.trim_start_matches('/').as_bytes())
        .map_err(named)
}

/// Opens a connection.
fn connect() -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open("/devices/fuse")
}

/// Mounts a filesystem in memory at [`DIR`] and serves it from a thread,
/// whose loop ends when it is unmounted.
fn serve() -> Option<JoinHandle<io::Result<()>>> {
    ok!(fs::create_dir_all(DIR))?;
    let device = ok!(connect())?;
    ok!(mount(&device))?;
    let mut session = Session::from_fd(MemFs::new(0, 0), OwnedFd::from(device), SessionACL::All);
    Some(thread::spawn(move || session.run()))
}

/// The path of `name` on the mount.
fn at(name: &str) -> String {
    format!("{DIR}/{name}")
}

/// The errno of a result's error, if it failed.
fn errno<T>(result: io::Result<T>) -> Option<i32> {
    result.err().and_then(|err| err.raw_os_error())
}

fn main() {
    let Some(served) = serve() else {
        println!("{NAME}: the filesystem was not mounted");
        std::process::exit(1);
    };
    part("mount: listed as a fuse filesystem from fuse-test", listed);
    part("files: made, written, read, appended to, cut short, renamed and removed", files);
    part("large: a 3 MiB file written and read back in the pieces INIT allows", large);
    part("directories: made, listed, moved and removed; a full one stays", directories);
    part("links: symbolic and hard links", links);
    part(
        "permissions: the program's owners and modes checked; another user refused, and theirs made",
        permissions,
    );
    part("nosuid: a set-ID bit kept by the program but never shown", nosuid);
    part("nodev: a device node on the mount opens nothing", nodev);
    part("unmount: the program reads ENODEV and ends; the folder is empty again", || {
        unmounted(served);
    });
    part("lost: with its program gone, the mount's calls fail as not connected", lost);
    let failures = FAILURES.load(Ordering::Relaxed);
    if failures == 0 {
        println!("{NAME}: every check passed");
    } else {
        println!("{NAME}: {failures} checks failed");
    }
    std::process::exit(i32::try_from(failures).unwrap_or(i32::MAX));
}

/// The mount is in the mounts list.
fn listed() {
    let Some(mounts) = ok!(fs::read_to_string("/processes/mounts")) else { return };
    let line = mounts.lines().find(|line| line.split(' ').nth(1) == Some(DIR));
    check!(line.is_some(), "{DIR} is not in /processes/mounts: {mounts}");
    if let Some(line) = line {
        let fields: Vec<&str> = line.split(' ').collect();
        check!(
            fields.first() == Some(&SOURCE) && fields.get(2) == Some(&"fuse"),
            "listed as {line}"
        );
        check!(fields.get(3) == Some(&"rw"), "listed as {line}");
    }
    let Some(root) = ok!(fs::metadata(DIR)) else { return };
    check!(root.is_dir() && root.mode() & 0o7777 == 0o755, "the root's mode is {:o}", root.mode());
}

fn files() {
    let path = at("hello.txt");
    check!(ok!(fs::write(&path, "hello\n")).is_some());
    check!(ok!(fs::read_to_string(&path)).as_deref() == Some("hello\n"));
    if let Some(mut file) = ok!(OpenOptions::new().append(true).open(&path)) {
        ok!(file.write_all(b"world\n"));
    }
    check!(ok!(fs::read_to_string(&path)).as_deref() == Some("hello\nworld\n"));
    // A write in the middle, and a read from it.
    if let Some(mut file) = ok!(OpenOptions::new().read(true).write(true).open(&path)) {
        ok!(file.seek(SeekFrom::Start(6)));
        ok!(file.write_all(b"W"));
        ok!(file.seek(SeekFrom::Start(6)));
        let mut word = [0; 5];
        ok!(file.read_exact(&mut word));
        check!(&word == b"World", "read {word:?}");
        // Cut short.
        ok!(file.set_len(5));
    }
    let meta = ok!(fs::metadata(&path));
    check!(meta.as_ref().is_some_and(|m| m.len() == 5 && m.is_file()), "{meta:?}");
    check!(ok!(fs::read_to_string(&path)).as_deref() == Some("hello"));
    // A new file's mode is what it was made with, less the umask.
    if let Some(file) =
        ok!(OpenOptions::new().write(true).create_new(true).mode(0o640).open(at("mode")))
    {
        let mode = ok!(file.metadata()).map_or(0, |m| m.mode() & 0o777);
        check!(mode == 0o640, "made with mode {mode:o}");
    }
    check!(
        errno(OpenOptions::new().write(true).create_new(true).open(at("mode")))
            == Some(libc::EEXIST)
    );
    ok!(fs::rename(&path, at("renamed.txt")));
    check!(errno(fs::metadata(&path)) == Some(libc::ENOENT));
    check!(ok!(fs::read_to_string(at("renamed.txt"))).as_deref() == Some("hello"));
    ok!(fs::remove_file(at("renamed.txt")));
    ok!(fs::remove_file(at("mode")));
    check!(errno(fs::metadata(at("renamed.txt"))) == Some(libc::ENOENT));
}

fn large() {
    let data: Vec<u8> =
        (0..3 << 20).map(|i: u32| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
    let path = at("large");
    check!(ok!(fs::write(&path, &data)).is_some());
    check!(ok!(fs::metadata(&path)).is_some_and(|m| m.len() == data.len() as u64));
    let read = ok!(fs::read(&path)).unwrap_or_default();
    check!(
        read == data,
        "read back {} bytes, {} differing",
        read.len(),
        first_difference(&read, &data)
    );
    ok!(fs::remove_file(&path));
}

/// Where two byte strings first differ, as text.
fn first_difference(a: &[u8], b: &[u8]) -> String {
    match a.iter().zip(b).position(|(x, y)| x != y) {
        Some(at) => format!("first at {at}"),
        None if a.len() == b.len() => "none".to_string(),
        None => format!("first at {}", a.len().min(b.len())),
    }
}

fn directories() {
    ok!(fs::create_dir_all(at("a/b/c")));
    ok!(fs::write(at("a/one"), "1"));
    ok!(fs::write(at("a/two"), "2"));
    let mut names: Vec<String> = ok!(fs::read_dir(at("a")))
        .map(|dir| {
            dir.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    check!(names == ["b", "one", "two"], "listed {names:?}");
    // Many entries, more than one READDIR answer holds.
    for i in 0..300 {
        ok!(fs::write(at(&format!("a/b/file-{i:03}")), ""));
    }
    let count = ok!(fs::read_dir(at("a/b"))).map_or(0, |dir| dir.filter_map(Result::ok).count());
    check!(count == 301, "listed {count} entries of 301");
    check!(errno(fs::remove_dir(at("a/b"))) == Some(libc::ENOTEMPTY));
    // A directory moved takes a link from its old parent and gives one
    // to its new: both are asked for again, kept as they were or not.
    check!(
        ok!(fs::metadata(at("a/b"))).is_some_and(|m| m.nlink() == 3),
        "b's links before the move"
    );
    let root = ok!(fs::metadata(DIR)).map_or(0, |m| m.nlink());
    ok!(fs::rename(at("a/b/c"), at("c")));
    check!(ok!(fs::metadata(at("c"))).is_some_and(|m| m.is_dir()));
    check!(
        ok!(fs::metadata(at("a/b"))).is_some_and(|m| m.nlink() == 2),
        "b's links after the move"
    );
    let moved = ok!(fs::metadata(DIR)).map_or(0, |m| m.nlink());
    check!(moved == root + 1, "the root's links went from {root} to {moved}");
    ok!(fs::remove_dir(at("c")));
    ok!(fs::remove_dir_all(at("a")));
    check!(errno(fs::metadata(at("a"))) == Some(libc::ENOENT));
}

fn links() {
    ok!(fs::write(at("target"), "linked"));
    ok!(symlink("target", at("soft")));
    check!(ok!(fs::read_link(at("soft"))).is_some_and(|t| t.as_os_str() == "target"));
    check!(ok!(fs::read_to_string(at("soft"))).as_deref() == Some("linked"));
    check!(ok!(fs::symlink_metadata(at("soft"))).is_some_and(|m| m.file_type().is_symlink()));
    ok!(fs::hard_link(at("target"), at("hard")));
    let (a, b) = (ok!(fs::metadata(at("target"))), ok!(fs::metadata(at("hard"))));
    check!(
        a.as_ref().zip(b.as_ref()).is_some_and(|(a, b)| a.ino() == b.ino() && a.nlink() == 2),
        "{a:?} {b:?}"
    );
    ok!(fs::remove_file(at("target")));
    check!(ok!(fs::read_to_string(at("hard"))).as_deref() == Some("linked"));
    check!(errno(fs::read_to_string(at("soft"))) == Some(libc::ENOENT));
    ok!(fs::remove_file(at("soft")));
    ok!(fs::remove_file(at("hard")));
}

/// `N` slots of the test's own: those of descriptors of /devices/null,
/// emptied once all are open, so that no open takes them; closing the
/// descriptors deletes what is put in them.
fn spares<const N: usize>() -> Option<([u32; N], Vec<File>)> {
    let files = ok!((0..N).map(|_| File::open("/devices/null")).collect::<io::Result<Vec<_>>>())?;
    let slots: [u32; N] =
        std::array::from_fn(|i| startup::SLOT_FD_BASE + files[i].as_raw_fd() as u32);
    for slot in slots {
        ok!(capability::delete(&Sys, slot).map_err(named))?;
    }
    Some((slots, files))
}

/// Puts in `to` a root that acts for user [`OTHER`] without the bypass
/// right, as a shell of theirs has one (ADR 0214), stamped in `scratch`.
fn other_root(scratch: u32, to: u32) -> Option<()> {
    ok!(names::stamp(&Sys, NAMES, startup::SLOT_ROOT, scratch, OTHER, OTHER, &[]).map_err(named))?;
    let rights = ok!(capability::info(&Sys, scratch).map_err(named))?.rights;
    let without = Rights(rights.bits() & !Rights::BYPASS.bits());
    ok!(capability::derive(&Sys, scratch, to, without, 0).map_err(named))?;
    ok!(capability::delete(&Sys, scratch).map_err(named))
}

/// Opens `name` on the mount through the root in `root`, into the empty
/// slot `file`, which is emptied again after: made with `mode` if
/// `create`, to read and write, and to read if not.
fn open_as(root: u32, file: u32, name: &str, create: bool, mode: u32) -> Result<(), Status> {
    let path = at(name);
    let (flags, rights) = if create {
        (OpenFlags::CREATE, Rights(Rights::READ.bits() | Rights::WRITE.bits()))
    } else {
        (OpenFlags::NONE, Rights::READ)
    };
    directory::open(
        &Sys,
        root,
        path.trim_start_matches('/').as_bytes(),
        flags,
        rights,
        file,
        mode,
    )?;
    capability::delete(&Sys, file)
}

/// Whether a status is the second gate's refusal (EACCES).
fn denied(opened: Result<(), Status>) -> bool {
    opened.is_err_and(|status| FileError::from_status(status) == Some(FileError::Denied))
}

fn permissions() {
    ok!(fs::write(at("private"), "root's"));
    ok!(fs::set_permissions(at("private"), fs::Permissions::from_mode(0o600)));
    ok!(fs::write(at("public"), "anyone's"));
    ok!(fs::set_permissions(at("public"), fs::Permissions::from_mode(0o644)));
    ok!(fs::create_dir(at("shared")));
    ok!(fs::set_permissions(at("shared"), fs::Permissions::from_mode(0o777)));
    let Some(([scratch, other, file], _spares)) = spares::<3>() else { return };
    if other_root(scratch, other).is_none() {
        return;
    }
    let public = open_as(other, file, "public", false, 0);
    check!(public.is_ok(), "another user cannot read a 0644 file: {:?}", public.map_err(named));
    let private = open_as(other, file, "private", false, 0);
    check!(
        denied(private),
        "another user read a 0600 file, or failed otherwise: {:?}",
        private.map_err(named)
    );
    // The root's folder is root's, 0755: they make nothing in it.
    check!(
        denied(open_as(other, file, "theirs", true, 0o644)),
        "another user made a file in a 0755 folder"
    );
    // In a folder open to all, what they make is theirs: the kernel told
    // the program who asked.
    let made = open_as(other, file, "shared/theirs", true, 0o640);
    check!(
        made.is_ok(),
        "another user cannot make a file in a 0777 folder: {:?}",
        made.map_err(named)
    );
    let meta = ok!(fs::metadata(at("shared/theirs")));
    check!(
        meta.as_ref()
            .is_some_and(|m| m.uid() == OTHER && m.gid() == OTHER && m.mode() & 0o777 == 0o640),
        "made as {meta:?}"
    );
    // Given to the other user, the private file is theirs to read.
    ok!(std::os::unix::fs::chown(at("private"), Some(OTHER), Some(OTHER)));
    check!(ok!(fs::metadata(at("private"))).is_some_and(|m| m.uid() == OTHER && m.gid() == OTHER));
    let given = open_as(other, file, "private", false, 0);
    check!(given.is_ok(), "the owner cannot read their 0600 file: {:?}", given.map_err(named));
    ok!(fs::remove_file(at("private")));
    ok!(fs::remove_file(at("public")));
    ok!(fs::remove_file(at("shared/theirs")));
    ok!(fs::remove_dir(at("shared")));
}

fn nosuid() {
    ok!(fs::write(at("setuid"), "#!/bin/sh\n"));
    ok!(fs::set_permissions(at("setuid"), fs::Permissions::from_mode(0o6755)));
    let mode = ok!(fs::metadata(at("setuid"))).map_or(0, |m| m.mode() & 0o7777);
    check!(mode == 0o755, "shown as {mode:o}");
    ok!(fs::remove_file(at("setuid")));
}

fn nodev() {
    let path = std::ffi::CString::new(at("null")).unwrap_or_default();
    // The kernel's null device, 1:3.
    // SAFETY: `path` is a string that ends in a NUL, which mknod only reads.
    let made = unsafe { libc::mknod(path.as_ptr(), libc::S_IFCHR | 0o666, libc::makedev(1, 3)) };
    check!(made == 0, "mknod: {}", io::Error::last_os_error());
    let meta = ok!(fs::metadata(at("null")));
    check!(
        meta.as_ref().is_some_and(
            |m| m.file_type().is_char_device() && m.rdev() == libc::makedev(1, 3) as u64
        ),
        "{meta:?}"
    );
    let opened = OpenOptions::new().read(true).open(at("null"));
    check!(errno(opened) == Some(libc::EACCES), "opened the device node, or failed otherwise");
    ok!(fs::remove_file(at("null")));
}

fn unmounted(served: JoinHandle<io::Result<()>>) {
    check!(ok!(unmount()).is_some());
    let deadline = Instant::now() + Duration::from_secs(20);
    while !served.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    check!(served.is_finished(), "the program's loop did not end");
    if served.is_finished() {
        let ended = served.join();
        check!(matches!(ended, Ok(Ok(()))), "the loop ended with {ended:?}");
    }
    let left = ok!(fs::read_dir(DIR)).map_or(usize::MAX, Iterator::count);
    check!(left == 0, "{left} files left in the folder");
}

fn lost() {
    let Some(device) = ok!(connect()) else { return };
    check!(ok!(mount(&device)).is_some());
    drop(device);
    // Opening the folder asks the program nothing; listing it does.
    let listed = fs::read_dir(DIR).and_then(Iterator::collect::<io::Result<Vec<_>>>);
    check!(
        errno(listed) == Some(libc::ENOTCONN),
        "listing the mount did not fail as not connected"
    );
    let written = fs::write(at("file"), "x");
    check!(written.as_ref().is_err_and(|err| err.kind() == ErrorKind::NotConnected), "{written:?}");
    check!(ok!(unmount()).is_some());
    check!(ok!(fs::read_dir(DIR)).is_some_and(|mut dir| dir.next().is_none()));
}
