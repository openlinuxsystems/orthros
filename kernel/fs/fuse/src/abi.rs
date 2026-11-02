// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Linux's FUSE protocol, version 7: the messages the kernel and a
//! filesystem's program exchange through `/devices/fuse`, written from
//! the protocol's description in Linux's `include/uapi/linux/fuse.h`
//! (whose terms allow the BSD licence for that one file) and checked
//! against fuser, the Rust library programs serve with.
//!
//! Every message starts with a header and is little-endian, as on x86:
//! a request with [`InHeader`] (40 bytes), then the operation's own
//! structure and names, each name ending in a NUL; an answer with
//! [`OutHeader`] (16 bytes), then the operation's answer unless the
//! header's error is not zero. The structures are those of protocol
//! 7.31, which the kernel offers; a program that answers `INIT` with an
//! older minor version from [`MINOR_MIN`] on gets the same messages,
//! but for `INIT`'s own answer, which is shorter.

use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Kind, Result};

/// The protocol's major version.
pub const MAJOR: u32 = 7;
/// The minor version the kernel offers.
pub const MINOR: u32 = 31;
/// The oldest minor version the kernel takes: the first whose structures
/// are all those of [`MINOR`] (`mknod` and `mkdir` with a umask, 7.12).
pub const MINOR_MIN: u32 = 12;

/// The node ID of the filesystem's root.
pub const ROOT: u64 = 1;

/// A request's header.
pub const IN_HEADER: usize = 40;
/// An answer's header.
pub const OUT_HEADER: usize = 16;

/// The operations, by their numbers.
#[allow(missing_docs)]
pub mod op {
    pub const LOOKUP: u32 = 1;
    pub const FORGET: u32 = 2;
    pub const GETATTR: u32 = 3;
    pub const SETATTR: u32 = 4;
    pub const READLINK: u32 = 5;
    pub const SYMLINK: u32 = 6;
    pub const MKNOD: u32 = 8;
    pub const MKDIR: u32 = 9;
    pub const UNLINK: u32 = 10;
    pub const RMDIR: u32 = 11;
    pub const RENAME: u32 = 12;
    pub const LINK: u32 = 13;
    pub const OPEN: u32 = 14;
    pub const READ: u32 = 15;
    pub const WRITE: u32 = 16;
    pub const STATFS: u32 = 17;
    pub const RELEASE: u32 = 18;
    pub const FSYNC: u32 = 20;
    pub const SETXATTR: u32 = 21;
    pub const GETXATTR: u32 = 22;
    pub const LISTXATTR: u32 = 23;
    pub const REMOVEXATTR: u32 = 24;
    pub const FLUSH: u32 = 25;
    pub const INIT: u32 = 26;
    pub const OPENDIR: u32 = 27;
    pub const READDIR: u32 = 28;
    pub const RELEASEDIR: u32 = 29;
    pub const CREATE: u32 = 35;
    pub const INTERRUPT: u32 = 36;
    pub const DESTROY: u32 = 38;
    pub const BATCH_FORGET: u32 = 42;
}

/// `INIT`'s flags the kernel offers.
pub mod init {
    /// Writes bigger than a page in one request.
    pub const BIG_WRITES: u32 = 1 << 5;
    /// The program applies no umask: the kernel already did.
    pub const DONT_MASK: u32 = 1 << 6;
    /// `max_pages` in the answer is the number of pages a request may
    /// carry.
    pub const MAX_PAGES: u32 = 1 << 22;
}

/// `SETATTR`'s bits that say what it sets.
#[allow(missing_docs)]
pub mod set {
    pub const MODE: u32 = 1 << 0;
    pub const UID: u32 = 1 << 1;
    pub const GID: u32 = 1 << 2;
    pub const SIZE: u32 = 1 << 3;
    pub const ATIME: u32 = 1 << 4;
    pub const MTIME: u32 = 1 << 5;
    pub const FH: u32 = 1 << 6;
    pub const CTIME: u32 = 1 << 10;
}

/// Linux's error numbers that the kernel tells apart in answers.
#[allow(missing_docs)]
pub mod errno {
    pub const EPERM: i32 = 1;
    pub const ENOENT: i32 = 2;
    pub const EINTR: i32 = 4;
    pub const EIO: i32 = 5;
    pub const ENXIO: i32 = 6;
    pub const E2BIG: i32 = 7;
    pub const EBADF: i32 = 9;
    pub const EAGAIN: i32 = 11;
    pub const ENOMEM: i32 = 12;
    pub const EACCES: i32 = 13;
    pub const EFAULT: i32 = 14;
    pub const EBUSY: i32 = 16;
    pub const EEXIST: i32 = 17;
    pub const EXDEV: i32 = 18;
    pub const ENODEV: i32 = 19;
    pub const ENOTDIR: i32 = 20;
    pub const EISDIR: i32 = 21;
    pub const EINVAL: i32 = 22;
    pub const EFBIG: i32 = 27;
    pub const ENOSPC: i32 = 28;
    pub const EROFS: i32 = 30;
    pub const EMLINK: i32 = 31;
    pub const ERANGE: i32 = 34;
    pub const ENAMETOOLONG: i32 = 36;
    pub const ENOSYS: i32 = 38;
    pub const ENOTEMPTY: i32 = 39;
    pub const ELOOP: i32 = 40;
    pub const ENODATA: i32 = 61;
    pub const EOPNOTSUPP: i32 = 95;
    pub const ENOTCONN: i32 = 107;
    pub const ETIMEDOUT: i32 = 110;
    pub const EHOSTUNREACH: i32 = 113;
    pub const EDQUOT: i32 = 122;
}

/// What an answer's error number means to the files' callers. Errors no
/// file call has a name for are input/output errors, as a disk's are;
/// a program that cannot reach its server says so (ENOTCONN).
pub fn error_of(errno: i32) -> Error {
    match errno {
        errno::EPERM => Error::NotPermitted,
        errno::ENOENT => Error::NotFound,
        errno::EINTR => Error::Interrupted,
        errno::ENOMEM => Error::NoMemory,
        errno::EACCES => Error::Denied,
        errno::EBUSY => Error::InUse,
        errno::EEXIST => Error::Exists,
        errno::EXDEV => Error::CrossMount,
        errno::ENODEV | errno::ENXIO => Error::NoDevice,
        errno::ENOTDIR => Error::NotDirectory,
        errno::EISDIR => Error::IsDirectory,
        errno::EINVAL => Error::InvalidArgument,
        errno::EFBIG | errno::E2BIG => Error::TooBig,
        errno::ENOSPC => Error::NoSpace,
        errno::EROFS => Error::ReadOnly,
        errno::EMLINK => Error::TooManyNames,
        errno::ERANGE => Error::OutOfRange,
        errno::ENAMETOOLONG => Error::NameTooLong,
        errno::ENOSYS | errno::EOPNOTSUPP => Error::NotSupported,
        errno::ENOTEMPTY => Error::NotEmpty,
        errno::ELOOP => Error::Loop,
        errno::ENODATA => Error::NoAttribute,
        errno::ENOTCONN | errno::ETIMEDOUT | errno::EHOSTUNREACH => Error::NotConnected,
        errno::EDQUOT => Error::QuotaExceeded,
        _ => Error::Io,
    }
}

/// Linux's `S_IFMT` and the kinds it tells apart in a mode.
#[allow(missing_docs)]
pub mod file_type {
    pub const MASK: u32 = 0o170_000;
    pub const SOCKET: u32 = 0o140_000;
    pub const LINK: u32 = 0o120_000;
    pub const FILE: u32 = 0o100_000;
    pub const BLOCK: u32 = 0o060_000;
    pub const DIRECTORY: u32 = 0o040_000;
    pub const CHAR: u32 = 0o020_000;
    pub const FIFO: u32 = 0o010_000;
}

/// The kind a mode says a node is; none for a mode of no kind.
pub fn kind_of(mode: u32) -> Option<Kind> {
    Some(match mode & file_type::MASK {
        file_type::FILE => Kind::File,
        file_type::DIRECTORY => Kind::Directory,
        file_type::LINK => Kind::Symlink,
        file_type::CHAR => Kind::CharDevice,
        file_type::BLOCK => Kind::BlockDevice,
        file_type::FIFO => Kind::Pipe,
        file_type::SOCKET => Kind::Socket,
        _ => return None,
    })
}

/// The mode bits of `kind`.
pub fn type_of(kind: Kind) -> u32 {
    match kind {
        Kind::File => file_type::FILE,
        Kind::Directory => file_type::DIRECTORY,
        Kind::Symlink => file_type::LINK,
        Kind::CharDevice => file_type::CHAR,
        Kind::BlockDevice => file_type::BLOCK,
        Kind::Pipe => file_type::FIFO,
        Kind::Socket => file_type::SOCKET,
    }
}

/// The kind a directory entry's type says (Linux's `DT_*`, the mode's
/// kind shifted down); none for an unknown one.
pub fn kind_of_entry(entry_type: u32) -> Option<Kind> {
    kind_of(entry_type << 12)
}

/// Builds a message: little-endian numbers and bytes, one after another.
pub struct Writer(pub KVec<u8>);

impl Writer {
    /// A message with room for `capacity` bytes.
    pub fn with_capacity(capacity: usize) -> Result<Writer> {
        Ok(Writer(mem::vec_with_capacity(capacity)?))
    }

    /// Adds `bytes`.
    pub fn bytes(&mut self, bytes: &[u8]) -> Result<&mut Writer> {
        mem::extend(&mut self.0, bytes)?;
        Ok(self)
    }

    /// Adds a 32-bit number.
    pub fn u32(&mut self, value: u32) -> Result<&mut Writer> {
        self.bytes(&value.to_le_bytes())
    }

    /// Adds a 64-bit number.
    pub fn u64(&mut self, value: u64) -> Result<&mut Writer> {
        self.bytes(&value.to_le_bytes())
    }

    /// Adds a name and the NUL that ends it.
    pub fn name(&mut self, name: &[u8]) -> Result<&mut Writer> {
        self.bytes(name)?.bytes(&[0])
    }
}

/// Reads a message: little-endian numbers, one after another. A message
/// shorter than what is read from it is the program's mistake, an
/// input/output error.
pub struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    /// Reads `bytes` from the start.
    pub fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes, at: 0 }
    }

    /// The next `len` bytes.
    pub fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(len).ok_or(Error::Io)?;
        let taken = self.bytes.get(self.at..end).ok_or(Error::Io)?;
        self.at = end;
        Ok(taken)
    }

    /// The next 16-bit number.
    pub fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    /// The next 32-bit number.
    pub fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// The next 64-bit number.
    pub fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        let mut v = [0u8; 8];
        v.copy_from_slice(b);
        Ok(u64::from_le_bytes(v))
    }

    /// What is left.
    pub fn rest(&self) -> &'a [u8] {
        &self.bytes[self.at..]
    }
}

/// A request's header (`fuse_in_header`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InHeader {
    /// The request's length, this header's 40 bytes included.
    pub len: u32,
    /// Its operation ([`op`]).
    pub opcode: u32,
    /// Its number, which the answer names; never 0.
    pub unique: u64,
    /// The node it is about.
    pub nodeid: u64,
    /// The user and group of the thread that made it, and its process.
    pub uid: u32,
    /// See [`InHeader::uid`].
    pub gid: u32,
    /// See [`InHeader::uid`].
    pub pid: u32,
}

impl InHeader {
    /// Writes it.
    pub fn write(&self, w: &mut Writer) -> Result<()> {
        w.u32(self.len)?.u32(self.opcode)?.u64(self.unique)?.u64(self.nodeid)?;
        // The last word is the extensions' length, 0: none are sent.
        w.u32(self.uid)?.u32(self.gid)?.u32(self.pid)?.u32(0)?;
        Ok(())
    }

    /// Reads one.
    pub fn read(r: &mut Reader<'_>) -> Result<InHeader> {
        let header = InHeader {
            len: r.u32()?,
            opcode: r.u32()?,
            unique: r.u64()?,
            nodeid: r.u64()?,
            uid: r.u32()?,
            gid: r.u32()?,
            pid: r.u32()?,
        };
        r.u32()?;
        Ok(header)
    }
}

/// An answer's header (`fuse_out_header`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutHeader {
    /// The answer's length, this header's 16 bytes included.
    pub len: u32,
    /// 0, or a negative error number, when nothing follows.
    pub error: i32,
    /// The number of the request it answers; 0 for a notification.
    pub unique: u64,
}

impl OutHeader {
    /// Reads one from the first 16 bytes of `bytes`.
    pub fn read(bytes: &[u8; OUT_HEADER]) -> OutHeader {
        let word = |at: usize| {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        OutHeader {
            len: word(0),
            error: word(4) as i32,
            unique: u64::from(word(8)) | u64::from(word(12)) << 32,
        }
    }

    /// Writes it.
    pub fn write(&self, w: &mut Writer) -> Result<()> {
        w.u32(self.len)?.u32(self.error as u32)?.u64(self.unique)?;
        Ok(())
    }
}

/// A node's attributes (`fuse_attr`, 88 bytes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attr {
    /// Its inode number: [`radix_vfs::Stat::id`].
    pub ino: u64,
    /// Its size in bytes.
    pub size: u64,
    /// The 512-byte blocks it takes.
    pub blocks: u64,
    /// When it was last read, in seconds since 1970.
    pub atime: u64,
    /// When its contents last changed.
    pub mtime: u64,
    /// When it last changed.
    pub ctime: u64,
    /// The nanoseconds of [`Attr::atime`].
    pub atimensec: u32,
    /// The nanoseconds of [`Attr::mtime`].
    pub mtimensec: u32,
    /// The nanoseconds of [`Attr::ctime`].
    pub ctimensec: u32,
    /// Its kind and permission bits.
    pub mode: u32,
    /// How many names it has.
    pub nlink: u32,
    /// Its owner.
    pub uid: u32,
    /// Its group.
    pub gid: u32,
    /// The device a device node stands for, as Linux encodes it.
    pub rdev: u32,
    /// The size of its blocks for I/O.
    pub blksize: u32,
}

impl Attr {
    /// The structure's size.
    pub const SIZE: usize = 88;

    /// Reads one.
    pub fn read(r: &mut Reader<'_>) -> Result<Attr> {
        let attr = Attr {
            ino: r.u64()?,
            size: r.u64()?,
            blocks: r.u64()?,
            atime: r.u64()?,
            mtime: r.u64()?,
            ctime: r.u64()?,
            atimensec: r.u32()?,
            mtimensec: r.u32()?,
            ctimensec: r.u32()?,
            mode: r.u32()?,
            nlink: r.u32()?,
            uid: r.u32()?,
            gid: r.u32()?,
            rdev: r.u32()?,
            blksize: r.u32()?,
        };
        // Flags (7.32), which no answer here uses.
        r.u32()?;
        Ok(attr)
    }

    /// Writes it, for the tests' programs.
    pub fn write(&self, w: &mut Writer) -> Result<()> {
        w.u64(self.ino)?.u64(self.size)?.u64(self.blocks)?;
        w.u64(self.atime)?.u64(self.mtime)?.u64(self.ctime)?;
        w.u32(self.atimensec)?.u32(self.mtimensec)?.u32(self.ctimensec)?;
        w.u32(self.mode)?.u32(self.nlink)?.u32(self.uid)?.u32(self.gid)?;
        w.u32(self.rdev)?.u32(self.blksize)?.u32(0)?;
        Ok(())
    }
}

/// A name's node, as `LOOKUP`, `CREATE`, `MKDIR`, `MKNOD`, `SYMLINK` and
/// `LINK` answer (`fuse_entry_out`, 128 bytes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// The node's ID, which the kernel names it by until it forgets it;
    /// 0 for a name that is not there, which the kernel need not forget.
    pub nodeid: u64,
    /// Which node of that ID it is, if IDs are used again.
    pub generation: u64,
    /// How long the name may be kept, in seconds and nanoseconds.
    pub entry_valid: u64,
    /// How long the attributes may be kept.
    pub attr_valid: u64,
    /// See [`Entry::entry_valid`].
    pub entry_valid_nsec: u32,
    /// See [`Entry::attr_valid`].
    pub attr_valid_nsec: u32,
    /// The node's attributes.
    pub attr: Attr,
}

impl Entry {
    /// The structure's size.
    pub const SIZE: usize = 40 + Attr::SIZE;

    /// Reads one.
    pub fn read(r: &mut Reader<'_>) -> Result<Entry> {
        Ok(Entry {
            nodeid: r.u64()?,
            generation: r.u64()?,
            entry_valid: r.u64()?,
            attr_valid: r.u64()?,
            entry_valid_nsec: r.u32()?,
            attr_valid_nsec: r.u32()?,
            attr: Attr::read(r)?,
        })
    }

    /// Writes it, for the tests' programs.
    pub fn write(&self, w: &mut Writer) -> Result<()> {
        w.u64(self.nodeid)?.u64(self.generation)?.u64(self.entry_valid)?.u64(self.attr_valid)?;
        w.u32(self.entry_valid_nsec)?.u32(self.attr_valid_nsec)?;
        self.attr.write(w)
    }
}

/// `GETATTR`'s and `SETATTR`'s answer (`fuse_attr_out`, 104 bytes): the
/// attributes and how long they may be kept, in seconds and nanoseconds.
pub fn read_attr_out(r: &mut Reader<'_>) -> Result<(Attr, u64, u32)> {
    let valid = r.u64()?;
    let valid_nsec = r.u32()?;
    r.u32()?;
    Ok((Attr::read(r)?, valid, valid_nsec))
}

/// What `INIT`'s answer says (`fuse_init_out`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InitOut {
    /// The program's major version.
    pub major: u32,
    /// Its minor version.
    pub minor: u32,
    /// The flags it takes of those offered.
    pub flags: u32,
    /// The most bytes it takes in one `WRITE`.
    pub max_write: u32,
    /// The most pages a request may carry, with [`init::MAX_PAGES`]; 0
    /// from a program of a minor version before 7.28.
    pub max_pages: u16,
}

impl InitOut {
    /// Reads `INIT`'s answer: 24 bytes from a program of a minor version
    /// before 7.23, more after.
    pub fn read(r: &mut Reader<'_>) -> Result<InitOut> {
        let major = r.u32()?;
        let minor = r.u32()?;
        let _max_readahead = r.u32()?;
        let flags = r.u32()?;
        let _max_background = r.u16()?;
        let _congestion_threshold = r.u16()?;
        let max_write = r.u32()?;
        // Then the time's granularity and, from 7.28 on, the pages.
        let max_pages = if r.rest().len() >= 6 {
            r.u32()?;
            r.u16()?
        } else {
            0
        };
        Ok(InitOut { major, minor, flags, max_write, max_pages })
    }
}

/// `STATFS`'s answer (`fuse_kstatfs`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatFs {
    /// Its blocks.
    pub blocks: u64,
    /// The free ones.
    pub bfree: u64,
    /// The free ones others than root may use.
    pub bavail: u64,
    /// Its files.
    pub files: u64,
    /// How many more it can take.
    pub ffree: u64,
    /// The size of its blocks.
    pub bsize: u32,
    /// The longest name it takes.
    pub namelen: u32,
    /// The size of its fragments, which `blocks` counts on Linux.
    pub frsize: u32,
}

impl StatFs {
    /// Reads one.
    pub fn read(r: &mut Reader<'_>) -> Result<StatFs> {
        Ok(StatFs {
            blocks: r.u64()?,
            bfree: r.u64()?,
            bavail: r.u64()?,
            files: r.u64()?,
            ffree: r.u64()?,
            bsize: r.u32()?,
            namelen: r.u32()?,
            frsize: r.u32()?,
        })
    }
}

/// A directory entry of `READDIR`'s answer (`fuse_dirent`): its inode
/// number, the offset that reads on after it, its type and its name.
pub struct DirEntry<'a> {
    /// Its inode number.
    pub ino: u64,
    /// The offset of the entry after it.
    pub off: u64,
    /// Its type: [`kind_of_entry`].
    pub entry_type: u32,
    /// Its name.
    pub name: &'a [u8],
}

/// The entries of `READDIR`'s answer, each padded to 8 bytes.
pub struct DirEntries<'a>(pub Reader<'a>);

/// The longest name a listing takes, as Linux's `FUSE_NAME_MAX`.
pub const NAME_MAX: usize = 1024;

impl<'a> Iterator for DirEntries<'a> {
    type Item = Result<DirEntry<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.0.rest().is_empty() {
            return None;
        }
        let mut entry = || -> Result<DirEntry<'a>> {
            let ino = self.0.u64()?;
            let off = self.0.u64()?;
            let namelen = self.0.u32()? as usize;
            let entry_type = self.0.u32()?;
            let name = self.0.take(namelen)?;
            // A name that is not one, as Linux refuses it: a path, which
            // a program walking the listing would follow out of the mount.
            if name.len() > NAME_MAX || name.contains(&b'/') || name.contains(&0) {
                return Err(Error::Io);
            }
            let pad = (24 + namelen).next_multiple_of(8) - (24 + namelen);
            // The last entry may stop short of its padding.
            let pad = pad.min(self.0.rest().len());
            self.0.take(pad)?;
            Ok(DirEntry { ino, off, entry_type, name })
        };
        let made = entry();
        if made.is_err() {
            // Stop at a broken entry.
            self.0 = Reader::new(&[]);
        }
        Some(made)
    }
}
