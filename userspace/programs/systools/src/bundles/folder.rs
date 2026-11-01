// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Folders open as descriptors, whose names are looked up one part at a
//! time and never through a link: bundled(8) works in folders of the
//! people it installs for, and the runner checks a sealed bundle's folder
//! (ADRs 0223 and 0225), so nothing there may lead elsewhere.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// The deepest a bundle's folders go.
pub const DEPTH_MAX: usize = 128;

/// A folder open as a descriptor. Each name is looked up beneath it, one
/// part only, never through a link.
pub struct Dir(pub OwnedFd);

/// `name` as one part of a path, as a C string.
pub fn c_name(name: &str) -> io::Result<CString> {
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{name:?}: not a name")));
    }
    CString::new(name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL"))
}

/// A C call's result, or the error it set.
pub fn check(result: libc::c_int) -> io::Result<()> {
    if result < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

impl Dir {
    /// Opens the folder at `path`, not through a link at its last part.
    pub fn open(path: &str) -> io::Result<Dir> {
        let path = CString::new(path).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: the path is a C string; open returns a descriptor we own.
        let fd = unsafe { libc::open(path.as_ptr(), flags) };
        check(fd)?;
        // SAFETY: open made it.
        Ok(Dir(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    /// Opens `name` beneath it, without following a link: a descriptor,
    /// for a folder or a file.
    pub fn open_at(&self, name: &str, folder: bool) -> io::Result<OwnedFd> {
        let name = c_name(name)?;
        let mut flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
        if folder {
            flags |= libc::O_DIRECTORY;
        }
        // SAFETY: both are valid; openat returns a descriptor we own.
        let fd = unsafe { libc::openat(self.0.as_raw_fd(), name.as_ptr(), flags) };
        check(fd)?;
        // SAFETY: openat made it.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Opens the file `name` beneath it to read and write, without
    /// following a link: what changing its owner and mode needs.
    pub fn open_writable(&self, name: &str) -> io::Result<OwnedFd> {
        let name = c_name(name)?;
        let flags = libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
        // SAFETY: both are valid; openat returns a descriptor we own.
        let fd = unsafe { libc::openat(self.0.as_raw_fd(), name.as_ptr(), flags) };
        check(fd)?;
        // SAFETY: openat made it.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Opens the folder `name` beneath it, not through a link.
    pub fn dir_at(&self, name: &str) -> io::Result<Dir> {
        self.open_at(name, true).map(Dir)
    }

    /// The folder's own owner, mode and identity.
    pub fn stat(&self) -> io::Result<libc::stat> {
        fd_stat(&self.0)
    }

    /// `name`'s, not through a link; none if there is no such name.
    pub fn stat_at(&self, name: &str) -> io::Result<Option<libc::stat>> {
        let name = c_name(name)?;
        // SAFETY: all zeros is a valid stat, which fstatat fills.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: the pointers are valid for the call.
        let r = unsafe {
            libc::fstatat(self.0.as_raw_fd(), name.as_ptr(), &raw mut st, libc::AT_SYMLINK_NOFOLLOW)
        };
        match check(r) {
            Ok(()) => Ok(Some(st)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn make(&self, name: &str, mode: libc::mode_t) -> io::Result<()> {
        let name = c_name(name)?;
        // SAFETY: the name is a C string.
        check(unsafe { libc::mkdirat(self.0.as_raw_fd(), name.as_ptr(), mode) })
    }

    pub fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let (from, to) = (c_name(from)?, c_name(to)?);
        let fd = self.0.as_raw_fd();
        // SAFETY: both names are C strings.
        check(unsafe { libc::renameat(fd, from.as_ptr(), fd, to.as_ptr()) })
    }

    /// The names in it, but `.` and `..`.
    pub fn names(&self) -> io::Result<Vec<String>> {
        // SAFETY: dup returns a descriptor we own, which fdopendir takes.
        let fd = unsafe { libc::dup(self.0.as_raw_fd()) };
        check(fd)?;
        // SAFETY: fd is ours; on failure we close it.
        let dir = unsafe { libc::fdopendir(fd) };
        if dir.is_null() {
            let e = io::Error::last_os_error();
            // SAFETY: fdopendir did not take it.
            unsafe { libc::close(fd) };
            return Err(e);
        }
        // SAFETY: rewinding a fresh stream, as dup shares the offset.
        unsafe { libc::rewinddir(dir) };
        let mut names = Vec::new();
        loop {
            // SAFETY: dir is open; readdir's entry lives until the next.
            let entry = unsafe { libc::readdir(dir) };
            if entry.is_null() {
                break;
            }
            // SAFETY: d_name is a C string in the entry.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            let name = name.to_string_lossy().into_owned();
            if name != "." && name != ".." {
                names.push(name);
            }
        }
        // SAFETY: closing the stream we opened, and its descriptor.
        unsafe { libc::closedir(dir) };
        Ok(names)
    }

    /// Makes the symbolic link `name` beneath it, leading to `target`.
    pub fn symlink(&self, target: &str, name: &str) -> io::Result<()> {
        let target =
            CString::new(target).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let name = c_name(name)?;
        // SAFETY: both are C strings.
        check(unsafe { libc::symlinkat(target.as_ptr(), self.0.as_raw_fd(), name.as_ptr()) })
    }

    /// Where the link `name` leads; none if `name` is not a link.
    pub fn read_link(&self, name: &str) -> io::Result<Option<String>> {
        let c = c_name(name)?;
        let mut buffer = [0u8; 4096];
        // SAFETY: the buffer is valid for its length.
        let n = unsafe {
            libc::readlinkat(
                self.0.as_raw_fd(),
                c.as_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            return if e.raw_os_error() == Some(libc::EINVAL) { Ok(None) } else { Err(e) };
        }
        let n = usize::try_from(n).unwrap_or(0);
        Ok(Some(String::from_utf8_lossy(&buffer[..n]).into_owned()))
    }

    /// Removes the file or link `name`.
    pub fn unlink(&self, name: &str) -> io::Result<()> {
        let c = c_name(name)?;
        // SAFETY: the name is a C string.
        check(unsafe { libc::unlinkat(self.0.as_raw_fd(), c.as_ptr(), 0) })
    }

    /// Reads the regular file `name` beneath it, not through a link, if it
    /// holds at most `max` bytes; with its owner and mode.
    pub fn read(&self, name: &str, max: u64) -> io::Result<(Vec<u8>, libc::stat)> {
        use std::io::Read;
        let fd = self.open_at(name, false)?;
        let st = fd_stat(&fd)?;
        if st.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(io::Error::other(format!("{name}: not a file")));
        }
        if u64::try_from(st.st_size).unwrap_or(u64::MAX) > max {
            return Err(io::Error::other(format!("{name}: too large")));
        }
        let mut bytes = Vec::new();
        std::fs::File::from(fd).take(max + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max {
            return Err(io::Error::other(format!("{name}: too large")));
        }
        Ok((bytes, st))
    }

    /// Removes `name` and all beneath it, not through a link.
    pub fn remove_tree(&self, name: &str, depth: usize) -> io::Result<()> {
        let Some(st) = self.stat_at(name)? else { return Ok(()) };
        let c = c_name(name)?;
        if st.st_mode & libc::S_IFMT == libc::S_IFDIR {
            if depth > DEPTH_MAX {
                return Err(io::Error::other("too deep"));
            }
            let inner = self.dir_at(name)?;
            for entry in inner.names()? {
                inner.remove_tree(&entry, depth + 1)?;
            }
            // SAFETY: the name is a C string.
            return check(unsafe {
                libc::unlinkat(self.0.as_raw_fd(), c.as_ptr(), libc::AT_REMOVEDIR)
            });
        }
        // SAFETY: the name is a C string.
        check(unsafe { libc::unlinkat(self.0.as_raw_fd(), c.as_ptr(), 0) })
    }
}

/// The owner, mode and identity of what `fd` holds.
pub fn fd_stat(fd: &OwnedFd) -> io::Result<libc::stat> {
    // SAFETY: all zeros is a valid stat, which fstat fills.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: the pointer is valid for the call.
    check(unsafe { libc::fstat(fd.as_raw_fd(), &raw mut st) })?;
    Ok(st)
}

/// Whether `a` and `b` are one file.
pub fn same(a: &libc::stat, b: &libc::stat) -> bool {
    a.st_dev == b.st_dev && a.st_ino == b.st_ino
}
