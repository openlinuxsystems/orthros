// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A filesystem in memory, served with fuser as any program written for
//! Linux's FUSE serves one: files, directories, symbolic and hard links,
//! and device nodes, each with the owner and mode it is given. It keeps
//! a set-ID bit it is given, which the kernel never shows (`nosuid`), and
//! makes the device nodes it is asked for, which the kernel never opens
//! (`nodev`).

use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
};
use libc::{EEXIST, EINVAL, EISDIR, ENOENT, ENOTDIR, ENOTEMPTY, EPERM, c_int};

/// How long the kernel may keep what it is told.
const TTL: Duration = Duration::from_secs(1);

/// The root's number, as the protocol fixes it.
const ROOT: u64 = 1;

/// A file, directory, link or device node.
struct Inode {
    attr: FileAttr,
    /// A file's bytes, or a symbolic link's target.
    data: Vec<u8>,
    /// A directory's entries.
    entries: BTreeMap<OsString, Named>,
}

/// A name in a directory: the node it stands for, and its place in a
/// listing, which it keeps while it is there, so that a listing goes on
/// rightly while names are taken away, as remove_dir_all does.
#[derive(Clone, Copy)]
struct Named {
    ino: u64,
    cookie: i64,
}

/// The filesystem.
pub struct MemFs {
    nodes: HashMap<u64, Inode>,
    next: u64,
    /// The next name's place in a listing: after `.` and `..`'s.
    cookie: i64,
}

/// A new node's attributes.
fn attributes(ino: u64, kind: FileType, perm: u16, req: &Request<'_>, rdev: u32) -> FileAttr {
    let now = SystemTime::now();
    FileAttr {
        ino,
        size: 0,
        blocks: 0,
        atime: now,
        mtime: now,
        ctime: now,
        crtime: now,
        kind,
        perm,
        nlink: if kind == FileType::Directory { 2 } else { 1 },
        uid: req.uid(),
        gid: req.gid(),
        rdev,
        blksize: 4096,
        flags: 0,
    }
}

/// The kind a mode's file-type bits name.
fn kind_of(mode: u32) -> Option<FileType> {
    Some(match mode & libc::S_IFMT {
        libc::S_IFREG => FileType::RegularFile,
        libc::S_IFDIR => FileType::Directory,
        libc::S_IFLNK => FileType::Symlink,
        libc::S_IFCHR => FileType::CharDevice,
        libc::S_IFBLK => FileType::BlockDevice,
        libc::S_IFIFO => FileType::NamedPipe,
        libc::S_IFSOCK => FileType::Socket,
        _ => return None,
    })
}

/// The permission bits of `mode`, less `umask`'s.
fn perm_of(mode: u32, umask: u32) -> u16 {
    (mode & !umask & 0o7777) as u16
}

impl MemFs {
    /// An empty filesystem, its root owned by `uid` and `gid`.
    pub fn new(uid: u32, gid: u32) -> MemFs {
        let now = SystemTime::now();
        let root = FileAttr {
            ino: ROOT,
            size: 0,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid,
            gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        };
        let mut nodes = HashMap::new();
        nodes.insert(ROOT, Inode { attr: root, data: Vec::new(), entries: BTreeMap::new() });
        MemFs { nodes, next: ROOT + 1, cookie: 3 }
    }

    /// The directory `ino`.
    fn directory(&mut self, ino: u64) -> Result<&mut Inode, c_int> {
        let node = self.nodes.get_mut(&ino).ok_or(ENOENT)?;
        if node.attr.kind != FileType::Directory {
            return Err(ENOTDIR);
        }
        Ok(node)
    }

    /// The number `name` in `parent` stands for.
    fn entry(&mut self, parent: u64, name: &OsStr) -> Result<u64, c_int> {
        self.directory(parent)?.entries.get(name).map(|named| named.ino).ok_or(ENOENT)
    }

    /// Makes a node named `name` in `parent`.
    fn make(
        &mut self,
        parent: u64,
        name: &OsStr,
        attr: FileAttr,
        data: Vec<u8>,
    ) -> Result<FileAttr, c_int> {
        let named = self.named(attr.ino);
        let dir = self.directory(parent)?;
        if dir.entries.contains_key(name) {
            return Err(EEXIST);
        }
        dir.entries.insert(name.to_os_string(), named);
        if attr.kind == FileType::Directory {
            dir.attr.nlink += 1;
        }
        dir.attr.mtime = SystemTime::now();
        self.nodes.insert(attr.ino, Inode { attr, data, entries: BTreeMap::new() });
        Ok(attr)
    }

    /// A new node's number.
    fn number(&mut self) -> u64 {
        self.next += 1;
        self.next - 1
    }

    /// A new name of `ino`, with the next place in listings.
    fn named(&mut self, ino: u64) -> Named {
        self.cookie += 1;
        Named { ino, cookie: self.cookie - 1 }
    }

    /// Takes away a name of `ino`, and the node with its last.
    fn drop_name(&mut self, ino: u64) {
        let Some(node) = self.nodes.get_mut(&ino) else { return };
        node.attr.nlink = node.attr.nlink.saturating_sub(1);
        node.attr.ctime = SystemTime::now();
        if node.attr.nlink == 0 || node.attr.kind == FileType::Directory {
            self.nodes.remove(&ino);
        }
    }

    /// Removes `name` from `parent`: a directory if `directory`, which
    /// must be empty, and anything else if not.
    fn remove(&mut self, parent: u64, name: &OsStr, directory: bool) -> Result<(), c_int> {
        let ino = self.entry(parent, name)?;
        let node = self.nodes.get(&ino).ok_or(ENOENT)?;
        let is_directory = node.attr.kind == FileType::Directory;
        if directory && !is_directory {
            return Err(ENOTDIR);
        }
        if !directory && is_directory {
            return Err(EISDIR);
        }
        if !node.entries.is_empty() {
            return Err(ENOTEMPTY);
        }
        let dir = self.directory(parent)?;
        dir.entries.remove(name);
        if is_directory {
            dir.attr.nlink -= 1;
        }
        dir.attr.mtime = SystemTime::now();
        self.drop_name(ino);
        Ok(())
    }

    /// Renames `name` in `parent` to `new_name` in `new_parent`, putting
    /// away what had that name.
    fn rename(
        &mut self,
        parent: u64,
        name: &OsStr,
        new_parent: u64,
        new_name: &OsStr,
    ) -> Result<(), c_int> {
        let ino = self.entry(parent, name)?;
        self.directory(new_parent)?;
        let is_directory = self.nodes.get(&ino).is_some_and(|n| n.attr.kind == FileType::Directory);
        if let Ok(old) = self.entry(new_parent, new_name) {
            if old == ino {
                return Ok(());
            }
            let old_is_directory =
                self.nodes.get(&old).is_some_and(|n| n.attr.kind == FileType::Directory);
            if is_directory != old_is_directory {
                return Err(if is_directory { ENOTDIR } else { EISDIR });
            }
            self.remove(new_parent, new_name, old_is_directory)?;
        }
        let named = self.named(ino);
        let from = self.directory(parent)?;
        from.entries.remove(name);
        if is_directory {
            from.attr.nlink -= 1;
        }
        let to = self.directory(new_parent)?;
        to.entries.insert(new_name.to_os_string(), named);
        if is_directory {
            to.attr.nlink += 1;
        }
        Ok(())
    }
}

impl Filesystem for MemFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        match self.entry(parent, name) {
            Ok(ino) => reply.entry(&TTL, &self.nodes[&ino].attr, 0),
            Err(err) => reply.error(err),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        match self.nodes.get(&ino) {
            Some(node) => reply.attr(&TTL, &node.attr),
            None => reply.error(ENOENT),
        }
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let Some(node) = self.nodes.get_mut(&ino) else { return reply.error(ENOENT) };
        let time = |t: TimeOrNow| match t {
            TimeOrNow::SpecificTime(t) => t,
            TimeOrNow::Now => SystemTime::now(),
        };
        if let Some(mode) = mode {
            node.attr.perm = (mode & 0o7777) as u16;
        }
        node.attr.uid = uid.unwrap_or(node.attr.uid);
        node.attr.gid = gid.unwrap_or(node.attr.gid);
        if let Some(size) = size {
            let Ok(size) = usize::try_from(size) else { return reply.error(EINVAL) };
            node.data.resize(size, 0);
            node.attr.size = size as u64;
            node.attr.blocks = node.attr.size.div_ceil(512);
            node.attr.mtime = SystemTime::now();
        }
        node.attr.atime = atime.map_or(node.attr.atime, time);
        node.attr.mtime = mtime.map_or(node.attr.mtime, time);
        node.attr.ctime = SystemTime::now();
        reply.attr(&TTL, &node.attr);
    }

    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        match self.nodes.get(&ino) {
            Some(node) if node.attr.kind == FileType::Symlink => reply.data(&node.data),
            Some(_) => reply.error(EINVAL),
            None => reply.error(ENOENT),
        }
    }

    fn mknod(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        let Some(kind) =
            kind_of(mode).filter(|kind| *kind != FileType::Directory && *kind != FileType::Symlink)
        else {
            return reply.error(EINVAL);
        };
        let attr = attributes(self.number(), kind, perm_of(mode, umask), req, rdev);
        match self.make(parent, name, attr, Vec::new()) {
            Ok(attr) => reply.entry(&TTL, &attr, 0),
            Err(err) => reply.error(err),
        }
    }

    fn mkdir(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let attr = attributes(self.number(), FileType::Directory, perm_of(mode, umask), req, 0);
        match self.make(parent, name, attr, Vec::new()) {
            Ok(attr) => reply.entry(&TTL, &attr, 0),
            Err(err) => reply.error(err),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        match self.remove(parent, name, false) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err),
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        match self.remove(parent, name, true) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err),
        }
    }

    fn symlink(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let target = target.as_os_str().as_bytes().to_vec();
        let mut attr = attributes(self.number(), FileType::Symlink, 0o777, req, 0);
        attr.size = target.len() as u64;
        match self.make(parent, link_name, attr, target) {
            Ok(attr) => reply.entry(&TTL, &attr, 0),
            Err(err) => reply.error(err),
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        if flags != 0 {
            return reply.error(EINVAL);
        }
        match self.rename(parent, name, newparent, newname) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err),
        }
    }

    fn link(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        newparent: u64,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let Some(node) = self.nodes.get(&ino) else { return reply.error(ENOENT) };
        if node.attr.kind == FileType::Directory {
            return reply.error(EPERM);
        }
        let named = self.named(ino);
        let dir = match self.directory(newparent) {
            Ok(dir) => dir,
            Err(err) => return reply.error(err),
        };
        if dir.entries.contains_key(newname) {
            return reply.error(EEXIST);
        }
        dir.entries.insert(newname.to_os_string(), named);
        let node = self.nodes.get_mut(&ino).expect("the node was found above");
        node.attr.nlink += 1;
        node.attr.ctime = SystemTime::now();
        reply.entry(&TTL, &node.attr, 0);
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        match self.nodes.get(&ino) {
            Some(node) if node.attr.kind == FileType::Directory => reply.error(EISDIR),
            Some(_) => reply.opened(0, 0),
            None => reply.error(ENOENT),
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let Some(node) = self.nodes.get(&ino) else { return reply.error(ENOENT) };
        let start = usize::try_from(offset).unwrap_or(usize::MAX).min(node.data.len());
        let end = start.saturating_add(size as usize).min(node.data.len());
        reply.data(&node.data[start..end]);
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let Some(node) = self.nodes.get_mut(&ino) else { return reply.error(ENOENT) };
        let Ok(start) = usize::try_from(offset) else { return reply.error(EINVAL) };
        let end = start + data.len();
        if node.data.len() < end {
            node.data.resize(end, 0);
        }
        node.data[start..end].copy_from_slice(data);
        node.attr.size = node.data.len() as u64;
        node.attr.blocks = node.attr.size.div_ceil(512);
        node.attr.mtime = SystemTime::now();
        reply.written(data.len() as u32);
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsync(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let Some(node) = self.nodes.get(&ino) else { return reply.error(ENOENT) };
        // Each entry's offset is its place, which the next READDIR starts
        // after: `.` is 1 and `..` 2.
        let mut listed = vec![
            (1, ino, FileType::Directory, OsString::from(".")),
            (2, ino, FileType::Directory, OsString::from("..")),
        ];
        for (name, named) in &node.entries {
            let kind = self.nodes.get(&named.ino).map_or(FileType::RegularFile, |n| n.attr.kind);
            listed.push((named.cookie, named.ino, kind, name.clone()));
        }
        listed.sort_unstable_by_key(|entry| entry.0);
        for (cookie, ino, kind, name) in listed.into_iter().filter(|entry| entry.0 > offset) {
            if reply.add(ino, cookie, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        let used: u64 = self.nodes.values().map(|n| n.attr.blocks).sum();
        let total = 1 << 20;
        let files = self.nodes.len() as u64;
        reply.statfs(
            total,
            total - used.min(total),
            total - used.min(total),
            files + 1000,
            1000,
            512,
            255,
            512,
        );
    }

    fn create(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let attr = attributes(self.number(), FileType::RegularFile, perm_of(mode, umask), req, 0);
        match self.make(parent, name, attr, Vec::new()) {
            Ok(attr) => reply.created(&TTL, &attr, 0, 0, 0),
            Err(err) => reply.error(err),
        }
    }
}
