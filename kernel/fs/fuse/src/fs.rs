// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The mounted filesystem and its nodes.
//!
//! A node stands for a node ID the program gave in a name's answer; one
//! node per ID while any reference to it lives, which a table of the
//! mount's nodes finds again. Each answer that names a node adds one to
//! the names the kernel holds of it, and when the node goes they are
//! given back with `FORGET`, as Linux counts them.
//!
//! A node keeps its attributes as long as the answer that brought them
//! says, then asks again (`GETATTR`). Names are asked for each time
//! (`LOOKUP`): nothing is kept of them.
//!
//! Reads and writes go to the program, a request of at most
//! [`Connection::max_data`] bytes at a time, through a file the node
//! opens the first time it reads, and one it opens the first time it
//! writes (`OPEN`), which stay open while the node lives and are closed
//! (`RELEASE`) when it goes: the kernel's descriptions of files are not
//! the program's. A file made with `CREATE` is open to write already.
//! Nothing is cached: each read asks, as Linux's `direct_io` does.

use alloc::sync::Weak;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use radix_sync::Mutex;
use radix_vfs::mem::{self, Alloc, KArc, KVec};
use radix_vfs::{
    APPEND, Access, AccessChange, Error, FileSystem, FsInfo, FsKind, In, Kind, ListFn, Node,
    NodeRef, Out, Result, Setting, Stat, acl,
};

use crate::abi::{self, Attr, Entry, Reader, StatFs, op};
use crate::conn::Connection;
use crate::hooks;

/// The most bytes of an extended attribute's value, or of a list of
/// their names, as on Linux.
const ATTRIBUTE_MAX: u32 = 65536;

/// How much of a directory one `READDIR` asks for.
const LIST_CHUNK: u32 = 8192;

/// Linux's open flags that requests carry.
mod open {
    pub const READ: u32 = 0;
    pub const WRITE: u32 = 1;
    pub const CREATE: u32 = 0o100;
    pub const EXCLUSIVE: u32 = 0o200;
    pub const DIRECTORY: u32 = 0o200_000;
}

/// What the mount and its nodes share.
struct Shared {
    conn: KArc<Connection>,
    /// The live nodes, by ID, in order of their IDs.
    nodes: Mutex<KVec<(u64, Weak<FuseNode, Alloc>)>>,
    /// Whether the program answered an extended attribute's request with
    /// ENOSYS: none is asked for again.
    no_attributes: AtomicBool,
    /// Whether it answered `CREATE` with ENOSYS: files are made with
    /// `MKNOD` and then opened.
    no_create: AtomicBool,
}

impl Shared {
    /// The node an answer names, with one more of its names held; `valid`
    /// is how long its attributes may be kept.
    fn node_for(shared: &KArc<Shared>, entry: &Entry) -> Result<KArc<FuseNode>> {
        if entry.nodeid == 0 {
            // A name that is not there, which the program may say so.
            return Err(Error::NotFound);
        }
        let made = Shared::find_or_make(shared, entry);
        if made.is_err() {
            // Never taken: given back at once.
            let _ = shared.conn.forget(entry.nodeid, 1);
        }
        made
    }

    fn find_or_make(shared: &KArc<Shared>, entry: &Entry) -> Result<KArc<FuseNode>> {
        let kind = abi::kind_of(entry.attr.mode).ok_or(Error::Io)?;
        if entry.nodeid == abi::ROOT {
            // The root is never named: a program that names it anyway
            // gets the mount's own, which is never forgotten.
            return Err(Error::Io);
        }
        let until = expiry(entry.attr_valid, entry.attr_valid_nsec);
        let mut nodes = shared.nodes.lock();
        let at = nodes.binary_search_by_key(&entry.nodeid, |(id, _)| *id);
        if let Ok(at) = at
            && let Some(node) = nodes[at].1.upgrade()
        {
            if node.kind == kind {
                node.lookups.fetch_add(1, Ordering::AcqRel);
                *node.cached.lock() = Cached { attr: entry.attr, until };
                return Ok(node);
            }
            // Another kind of node under the same ID: the program made a
            // new one, which takes the old one's place in the table; the
            // old one gives its names back when it goes.
            drop(nodes);
            drop(node);
            nodes = shared.nodes.lock();
        }
        let at = nodes.binary_search_by_key(&entry.nodeid, |(id, _)| *id);
        let node = mem::arc(FuseNode {
            shared: shared.clone(),
            nodeid: entry.nodeid,
            kind,
            lookups: AtomicU64::new(1),
            cached: Mutex::new(Cached { attr: entry.attr, until }),
            files: Mutex::new(Files::default()),
        })?;
        let weak = KArc::downgrade(&node);
        let placed = match at {
            // One going away: its entry is this one's now.
            Ok(at) => {
                nodes[at].1 = weak;
                Ok(())
            }
            Err(at) => mem::insert(&mut nodes, at, (entry.nodeid, weak)),
        };
        // A node goes with the table unlocked: its `drop` takes the lock.
        drop(nodes);
        if placed.is_err() {
            // The caller gives the name back.
            node.lookups.store(0, Ordering::Release);
            return Err(Error::NoMemory);
        }
        Ok(node)
    }

    /// The node `node` stands for, if it is one of this mount's.
    fn own<'a>(shared: &KArc<Shared>, node: &'a dyn Node) -> Result<&'a FuseNode> {
        let theirs = radix_vfs::downcast::<FuseNode>(node).ok_or(Error::CrossMount)?;
        if KArc::ptr_eq(&theirs.shared, shared) { Ok(theirs) } else { Err(Error::CrossMount) }
    }
}

/// When attributes kept now for `secs` and `nsecs` stop being good.
fn expiry(secs: u64, nsecs: u32) -> u64 {
    let ns = secs.saturating_mul(1_000_000_000).saturating_add(u64::from(nsecs));
    (hooks().now)().saturating_add(ns)
}

/// Nanoseconds since 1970 from seconds and nanoseconds.
fn time(secs: u64, nsecs: u32) -> u64 {
    secs.saturating_mul(1_000_000_000).saturating_add(u64::from(nsecs))
}

/// Seconds and nanoseconds from nanoseconds since 1970.
fn split(ns: u64) -> (u64, u32) {
    (ns / 1_000_000_000, (ns % 1_000_000_000) as u32)
}

/// A device number as Linux encodes it in 32 bits, as [`Stat::device`]
/// holds it.
fn device(rdev: u32) -> u64 {
    let major = (rdev & 0xf_ff00) >> 8;
    let minor = (rdev & 0xff) | ((rdev >> 12) & 0xf_ff00);
    radix_vfs::device_number(major, minor)
}

/// A device number as Linux encodes it in 32 bits.
fn rdev(device: u64) -> u32 {
    let (major, minor) = radix_vfs::device_parts(device);
    (minor & 0xff) | ((major & 0xfff) << 8) | ((minor & !0xff) << 12)
}

/// A node's attributes and when they stop being good.
#[derive(Clone, Copy)]
struct Cached {
    attr: Attr,
    until: u64,
}

/// The program's files a node has open.
#[derive(Default)]
struct Files {
    read: Option<u64>,
    write: Option<u64>,
}

/// A mounted filesystem a program serves.
pub struct FuseFs {
    shared: KArc<Shared>,
    root: KArc<FuseNode>,
    read_only: bool,
    /// What `STATFS` last said.
    info: Mutex<FsInfo>,
}

impl FuseFs {
    /// Mounts `conn`, read-only if `read_only`: queues `INIT`, which the
    /// first call waits for. Fails with [`Error::InUse`] if it was
    /// mounted before.
    pub fn mount(conn: &KArc<Connection>, read_only: bool) -> Result<KArc<FuseFs>> {
        let shared = mem::arc(Shared {
            conn: conn.clone(),
            nodes: Mutex::new(mem::vec()),
            no_attributes: AtomicBool::new(false),
            no_create: AtomicBool::new(false),
        })?;
        let attr = Attr {
            ino: abi::ROOT,
            mode: abi::file_type::DIRECTORY | 0o755,
            nlink: 2,
            ..Attr::default()
        };
        let root = mem::arc(FuseNode {
            shared: shared.clone(),
            nodeid: abi::ROOT,
            kind: Kind::Directory,
            // The root is not named, so never forgotten.
            lookups: AtomicU64::new(0),
            cached: Mutex::new(Cached { attr, until: 0 }),
            files: Mutex::new(Files::default()),
        })?;
        conn.mount()?;
        // From here the mount owns the connection: one that is not made
        // lets the program go.
        let made =
            mem::arc(FuseFs { shared, root, read_only, info: Mutex::new(FsInfo::default()) });
        if made.is_err() {
            conn.unmount();
        }
        Ok(made?)
    }

    /// Its connection.
    pub fn connection(&self) -> &KArc<Connection> {
        &self.shared.conn
    }
}

impl Drop for FuseFs {
    fn drop(&mut self) {
        self.shared.conn.unmount();
    }
}

impl FileSystem for FuseFs {
    fn kind(&self) -> FsKind {
        FsKind::Fuse
    }

    fn root(&self) -> NodeRef {
        self.root.clone()
    }

    fn info(&self) -> FsInfo {
        let asked = self.shared.conn.call(op::STATFS, abi::ROOT, None, &[], None);
        let mut info = self.info.lock();
        if let Ok(data) = asked
            && let Ok(s) = StatFs::read(&mut Reader::new(&data))
        {
            let block_size = if s.frsize != 0 { s.frsize } else { s.bsize };
            *info = FsInfo {
                block_size,
                blocks: s.blocks,
                free: s.bavail,
                files: s.files,
                free_files: s.ffree,
            };
        }
        *info
    }

    fn read_only(&self) -> bool {
        self.read_only
    }
}

/// A node of a mounted filesystem a program serves.
pub struct FuseNode {
    shared: KArc<Shared>,
    nodeid: u64,
    kind: Kind,
    /// How many of its names the kernel holds.
    lookups: AtomicU64,
    cached: Mutex<Cached>,
    files: Mutex<Files>,
}

impl FuseNode {
    /// Its node ID.
    pub fn nodeid(&self) -> u64 {
        self.nodeid
    }

    fn conn(&self) -> &Connection {
        &self.shared.conn
    }

    /// Sends a request about this node and waits for the answer.
    fn call(&self, opcode: u32, parts: &[&[u8]]) -> Result<KVec<u8>> {
        self.conn().call(opcode, self.nodeid, None, parts, None)
    }

    /// Its attributes: those kept while they are good, or asked for. A
    /// program that cannot answer leaves those kept.
    fn attr(&self) -> Attr {
        let kept = *self.cached.lock();
        if (hooks().now)() < kept.until {
            return kept.attr;
        }
        self.fetch().unwrap_or(kept.attr)
    }

    /// Asks for its attributes now.
    fn fetch(&self) -> Result<Attr> {
        let data = self.call(op::GETATTR, &[&[0u8; 16]])?;
        self.keep(&data)
    }

    /// Keeps the attributes of a `GETATTR` or `SETATTR` answer.
    fn keep(&self, data: &[u8]) -> Result<Attr> {
        let (attr, valid, valid_nsec) = abi::read_attr_out(&mut Reader::new(data))?;
        let mut kept = self.cached.lock();
        if abi::kind_of(attr.mode) == Some(self.kind) {
            *kept = Cached { attr, until: expiry(valid, valid_nsec) };
        }
        Ok(kept.attr)
    }

    /// Lets the attributes be asked for again: they have changed.
    fn stale(&self) {
        self.cached.lock().until = 0;
    }

    /// The program's file of this node open to read, or to write: opened
    /// now if it is not open yet.
    fn file(&self, write: bool) -> Result<u64> {
        match self.kind {
            Kind::File => {}
            Kind::Directory => return Err(Error::IsDirectory),
            _ => return Err(Error::InvalidArgument),
        }
        let mut files = self.files.lock();
        let held = if write { &mut files.write } else { &mut files.read };
        if let Some(fh) = *held {
            return Ok(fh);
        }
        let flags = if write { open::WRITE } else { open::READ };
        let mut body = [0u8; 8];
        body[..4].copy_from_slice(&flags.to_le_bytes());
        let data = self.call(op::OPEN, &[&body])?;
        let fh = Reader::new(&data).u64()?;
        *held = Some(fh);
        Ok(fh)
    }

    /// Sets what `valid` names to the values in `set` (`SETATTR`).
    fn set(&self, valid: u32, set: &SetAttr) -> Result<()> {
        let write = self.files.lock().write;
        let (valid, fh) = match write {
            Some(fh) if valid & abi::set::SIZE != 0 => (valid | abi::set::FH, fh),
            _ => (valid, 0),
        };
        let mut body = [0u8; 88];
        let (atime, atimensec) = split(set.accessed);
        let (mtime, mtimensec) = split(set.modified);
        body[0..4].copy_from_slice(&valid.to_le_bytes());
        body[8..16].copy_from_slice(&fh.to_le_bytes());
        body[16..24].copy_from_slice(&set.size.to_le_bytes());
        body[32..40].copy_from_slice(&atime.to_le_bytes());
        body[40..48].copy_from_slice(&mtime.to_le_bytes());
        body[56..60].copy_from_slice(&atimensec.to_le_bytes());
        body[60..64].copy_from_slice(&mtimensec.to_le_bytes());
        body[68..72].copy_from_slice(&set.mode.to_le_bytes());
        body[76..80].copy_from_slice(&set.uid.to_le_bytes());
        body[80..84].copy_from_slice(&set.gid.to_le_bytes());
        let data = self.call(op::SETATTR, &[&body])?;
        self.keep(&data).map(drop)
    }

    /// A name's node, from an answer that names one.
    fn named(&self, data: &[u8]) -> Result<NodeRef> {
        let entry = Entry::read(&mut Reader::new(data))?;
        let node: NodeRef = Shared::node_for(&self.shared, &entry)?;
        Ok(node)
    }

    /// Makes `name` with `opcode` (`MKDIR`, `MKNOD` or `SYMLINK`) and
    /// `parts`, as `access`'s owner and group.
    fn make(&self, opcode: u32, access: Access, parts: &[&[u8]]) -> Result<NodeRef> {
        self.directory()?;
        let data = self.conn().call(
            opcode,
            self.nodeid,
            Some((access.owner, access.group)),
            parts,
            None,
        )?;
        // A new name: the directory's size, times and, for a directory
        // made, its links have changed, as Linux's FUSE takes them to.
        self.stale();
        self.named(&data)
    }

    fn directory(&self) -> Result<()> {
        if self.kind == Kind::Directory { Ok(()) } else { Err(Error::NotDirectory) }
    }

    /// Makes the file `name` with `CREATE`, open to write; with `MKNOD`
    /// if the program has no `CREATE`.
    fn create_file(&self, name: &str, access: Access) -> Result<NodeRef> {
        let mode = abi::file_type::FILE | u32::from(access.mode);
        if !self.shared.no_create.load(Ordering::Acquire) {
            let mut body = [0u8; 16];
            let flags = open::WRITE | open::CREATE | open::EXCLUSIVE;
            body[0..4].copy_from_slice(&flags.to_le_bytes());
            body[4..8].copy_from_slice(&mode.to_le_bytes());
            let who = Some((access.owner, access.group));
            match self.conn().call(
                op::CREATE,
                self.nodeid,
                who,
                &[&body, name.as_bytes(), &[0]],
                None,
            ) {
                Ok(data) => {
                    self.stale();
                    let node = self.named(&data)?;
                    let fh = Reader::new(data.get(Entry::SIZE..).ok_or(Error::Io)?).u64()?;
                    let file = radix_vfs::downcast::<FuseNode>(&*node).ok_or(Error::Io)?;
                    let mut files = file.files.lock();
                    if files.write.is_none() {
                        files.write = Some(fh);
                    } else {
                        let _ = self.conn().release(op::RELEASE, file.nodeid, fh, open::WRITE);
                    }
                    drop(files);
                    return Ok(node);
                }
                Err(Error::NotSupported) => self.shared.no_create.store(true, Ordering::Release),
                Err(err) => return Err(err),
            }
        }
        let mut body = [0u8; 16];
        body[0..4].copy_from_slice(&mode.to_le_bytes());
        self.make(op::MKNOD, access, &[&body, name.as_bytes(), &[0]])
    }

    /// Sends an extended attribute's request, unless the program has
    /// none.
    fn attribute_call(&self, opcode: u32, parts: &[&[u8]]) -> Result<KVec<u8>> {
        if self.shared.no_attributes.load(Ordering::Acquire) {
            return Err(Error::NotSupported);
        }
        let asked = self.call(opcode, parts);
        if matches!(asked, Err(Error::NotSupported)) {
            self.shared.no_attributes.store(true, Ordering::Release);
        }
        asked
    }

    /// Reads a list of names that each end in a NUL.
    fn listed(&self, opcode: u32, name: &[u8]) -> Result<KVec<u8>> {
        let mut body = [0u8; 8];
        body[..4].copy_from_slice(&ATTRIBUTE_MAX.to_le_bytes());
        if name.is_empty() {
            self.attribute_call(opcode, &[&body])
        } else {
            self.attribute_call(opcode, &[&body, name, &[0]])
        }
    }
}

/// What `SETATTR` sets, of the values its bits name.
#[derive(Default)]
struct SetAttr {
    size: u64,
    accessed: u64,
    modified: u64,
    mode: u32,
    uid: u32,
    gid: u32,
}

impl Drop for FuseNode {
    fn drop(&mut self) {
        let conn = &self.shared.conn;
        let files = core::mem::take(self.files.get_mut());
        if let Some(fh) = files.read {
            let _ = conn.release(op::RELEASE, self.nodeid, fh, open::READ);
        }
        if let Some(fh) = files.write {
            let _ = conn.release(op::RELEASE, self.nodeid, fh, open::WRITE);
        }
        {
            let me = core::ptr::from_ref(self);
            let mut nodes = self.shared.nodes.lock();
            if let Ok(at) = nodes.binary_search_by_key(&self.nodeid, |(id, _)| *id)
                && core::ptr::eq(nodes[at].1.as_ptr(), me)
            {
                nodes.remove(at);
            }
        }
        let lookups = *self.lookups.get_mut();
        if lookups > 0 {
            let _ = conn.forget(self.nodeid, lookups);
        }
    }
}

impl Node for FuseNode {
    fn stat(&self) -> Stat {
        let a = self.attr();
        Stat {
            kind: self.kind,
            size: a.size,
            allocated: a.blocks.saturating_mul(512),
            id: a.ino,
            created: 0,
            modified: time(a.mtime, a.mtimensec),
            accessed: time(a.atime, a.atimensec),
            changed: time(a.ctime, a.ctimensec),
            links: a.nlink,
            device: device(a.rdev),
        }
    }

    fn kind(&self) -> Kind {
        self.kind
    }

    fn access(&self) -> Access {
        let a = self.attr();
        // Never set-ID: a program's files run as whoever runs them
        // (`nosuid`).
        let mode =
            (a.mode & 0o7777) as u16 & !(radix_vfs::mode::SET_USER | radix_vfs::mode::SET_GROUP);
        Access { owner: a.uid, group: a.gid, mode }
    }

    fn set_access(&self, change: &AccessChange) -> Result<()> {
        let a = self.attr();
        let now = Access { owner: a.uid, group: a.gid, mode: (a.mode & 0o7777) as u16 };
        let new = change.apply(now);
        let mut valid = 0;
        if change.mode.is_some() || new.mode != now.mode {
            valid |= abi::set::MODE;
        }
        if change.owner.is_some() {
            valid |= abi::set::UID;
        }
        if change.group.is_some() {
            valid |= abi::set::GID;
        }
        let set = SetAttr {
            mode: u32::from(new.mode),
            uid: new.owner,
            gid: new.group,
            ..SetAttr::default()
        };
        self.set(valid, &set)
    }

    fn lookup(&self, name: &str) -> Result<NodeRef> {
        self.directory()?;
        let data = self.call(op::LOOKUP, &[name.as_bytes(), &[0]])?;
        self.named(&data)
    }

    fn create(&self, name: &str, kind: Kind, access: Access) -> Result<NodeRef> {
        self.directory()?;
        if kind == Kind::Directory {
            let mut body = [0u8; 8];
            body[..4].copy_from_slice(&u32::from(access.mode).to_le_bytes());
            self.make(op::MKDIR, access, &[&body, name.as_bytes(), &[0]])
        } else {
            self.create_file(name, access)
        }
    }

    fn make_link(&self, name: &str, target: &str, access: Access) -> Result<NodeRef> {
        self.make(op::SYMLINK, access, &[name.as_bytes(), &[0], target.as_bytes(), &[0]])
    }

    fn link(&self, name: &str, node: &NodeRef) -> Result<()> {
        self.directory()?;
        let theirs = Shared::own(&self.shared, &**node)?;
        let data = self.call(op::LINK, &[&theirs.nodeid.to_le_bytes(), name.as_bytes(), &[0]])?;
        // The answer names it: one more of its names held.
        let named = self.named(&data)?;
        theirs.stale();
        self.stale();
        drop(named);
        Ok(())
    }

    fn make_node(&self, name: &str, kind: Kind, device: u64, access: Access) -> Result<NodeRef> {
        let mut body = [0u8; 16];
        let mode = abi::type_of(kind) | u32::from(access.mode);
        body[0..4].copy_from_slice(&mode.to_le_bytes());
        body[4..8].copy_from_slice(&rdev(device).to_le_bytes());
        self.make(op::MKNOD, access, &[&body, name.as_bytes(), &[0]])
    }

    fn read_link(&self, out: &mut dyn Out) -> Result<usize> {
        if self.kind != Kind::Symlink {
            return Err(Error::NotLink);
        }
        let target = self.call(op::READLINK, &[])?;
        let n = target.len().min(out.len());
        out.put(0, &target[..n])?;
        Ok(target.len())
    }

    fn remove(&self, name: &str) -> Result<()> {
        self.directory()?;
        let parts: [&[u8]; 2] = [name.as_bytes(), &[0]];
        let removed = match self.call(op::UNLINK, &parts) {
            Err(err @ (Error::IsDirectory | Error::NotPermitted)) => {
                match self.call(op::RMDIR, &parts) {
                    Err(Error::NotDirectory) => Err(err),
                    other => other.map(drop),
                }
            }
            other => other.map(drop),
        };
        if removed.is_ok() {
            self.stale();
        }
        removed
    }

    fn rename(&self, from: &str, to_dir: &dyn Node, to: &str) -> Result<()> {
        self.directory()?;
        let target = Shared::own(&self.shared, to_dir)?;
        let parts: [&[u8]; 5] =
            [&target.nodeid.to_le_bytes(), from.as_bytes(), &[0], to.as_bytes(), &[0]];
        self.call(op::RENAME, &parts)?;
        // Both directories have changed: a directory moved takes a link
        // from one and gives one to the other.
        self.stale();
        target.stale();
        Ok(())
    }

    fn list(&self, cursor: u64, f: ListFn<'_>) -> Result<()> {
        self.directory()?;
        let mut body = [0u8; 8];
        body[..4].copy_from_slice(&(open::READ | open::DIRECTORY).to_le_bytes());
        let data = self.call(op::OPENDIR, &[&body])?;
        let fh = Reader::new(&data).u64()?;
        let listed = (|| -> Result<()> {
            let mut cursor = cursor;
            loop {
                let mut read = [0u8; 40];
                read[0..8].copy_from_slice(&fh.to_le_bytes());
                read[8..16].copy_from_slice(&cursor.to_le_bytes());
                read[16..20].copy_from_slice(&LIST_CHUNK.to_le_bytes());
                read[32..36].copy_from_slice(&(open::READ | open::DIRECTORY).to_le_bytes());
                let data = self.call(op::READDIR, &[&read])?;
                let mut moved = false;
                for entry in abi::DirEntries(Reader::new(&data)) {
                    let entry = entry?;
                    if entry.off == cursor {
                        continue;
                    }
                    cursor = entry.off;
                    moved = true;
                    if matches!(entry.name, b"." | b"..") || entry.name.is_empty() {
                        continue;
                    }
                    // A name that is not UTF-8 cannot be named in a path
                    // here: it is left out.
                    let Ok(name) = core::str::from_utf8(entry.name) else { continue };
                    let kind = abi::kind_of_entry(entry.entry_type).unwrap_or(Kind::File);
                    if !f(entry.off, name, kind) {
                        return Ok(());
                    }
                }
                if !moved {
                    return Ok(());
                }
            }
        })();
        let _ = self.conn().release(op::RELEASEDIR, self.nodeid, fh, open::READ | open::DIRECTORY);
        listed
    }

    fn read(&self, offset: u64, out: &mut dyn Out) -> Result<usize> {
        let fh = self.file(false)?;
        let max = self.conn().max_data() as usize;
        let mut done = 0;
        while done < out.len() {
            let want = (out.len() - done).min(max);
            let at = offset.checked_add(done as u64).ok_or(Error::TooBig)?;
            let mut read = [0u8; 40];
            read[0..8].copy_from_slice(&fh.to_le_bytes());
            read[8..16].copy_from_slice(&at.to_le_bytes());
            read[16..20].copy_from_slice(&(want as u32).to_le_bytes());
            let data = self.call(op::READ, &[&read])?;
            if data.len() > want {
                return Err(Error::Io);
            }
            out.put(done, &data)?;
            done += data.len();
            if data.len() < want {
                break;
            }
        }
        Ok(done)
    }

    fn write(&self, offset: u64, data: &dyn In) -> Result<usize> {
        let fh = self.file(true)?;
        let offset = if offset == APPEND { self.fetch()?.size } else { offset };
        let max = self.conn().max_data() as usize;
        let mut done = 0;
        while done < data.len() {
            let len = (data.len() - done).min(max);
            let at = offset.checked_add(done as u64).ok_or(Error::TooBig)?;
            let mut head = [0u8; 40];
            head[0..8].copy_from_slice(&fh.to_le_bytes());
            head[8..16].copy_from_slice(&at.to_le_bytes());
            head[16..20].copy_from_slice(&(len as u32).to_le_bytes());
            head[32..36].copy_from_slice(&open::WRITE.to_le_bytes());
            let answer =
                self.conn().call(op::WRITE, self.nodeid, None, &[&head], Some((data, done, len)));
            let written = match answer {
                Ok(answer) => Reader::new(&answer).u32()? as usize,
                Err(err) if done == 0 => return Err(err),
                Err(_) => break,
            };
            if written > len {
                return Err(Error::Io);
            }
            done += written;
            if written < len {
                break;
            }
        }
        self.stale();
        Ok(done)
    }

    fn set_size(&self, size: u64) -> Result<()> {
        if self.kind == Kind::Directory {
            return Err(Error::IsDirectory);
        }
        self.set(abi::set::SIZE, &SetAttr { size, ..SetAttr::default() })
    }

    fn set_times(&self, accessed: Option<u64>, modified: Option<u64>) -> Result<()> {
        let mut valid = 0;
        if accessed.is_some() {
            valid |= abi::set::ATIME;
        }
        if modified.is_some() {
            valid |= abi::set::MTIME;
        }
        if valid == 0 {
            return Ok(());
        }
        let set = SetAttr {
            accessed: accessed.unwrap_or(0),
            modified: modified.unwrap_or(0),
            ..SetAttr::default()
        };
        self.set(valid, &set)
    }

    fn get_attribute(&self, name: &[u8]) -> Result<KVec<u8>> {
        self.listed(op::GETXATTR, name)
    }

    fn set_attribute(&self, name: &[u8], value: &[u8], how: Setting) -> Result<()> {
        let flags: u32 = match how {
            Setting::Either => 0,
            Setting::Create => 1,
            Setting::Replace => 2,
        };
        let size = u32::try_from(value.len()).map_err(|_| Error::OutOfRange)?;
        let mut body = [0u8; 8];
        body[..4].copy_from_slice(&size.to_le_bytes());
        body[4..8].copy_from_slice(&flags.to_le_bytes());
        self.attribute_call(op::SETXATTR, &[&body, name, &[0], value]).map(drop)?;
        self.stale();
        Ok(())
    }

    fn list_attributes(&self) -> Result<KVec<u8>> {
        self.listed(op::LISTXATTR, &[])
    }

    fn remove_attribute(&self, name: &[u8]) -> Result<()> {
        self.attribute_call(op::REMOVEXATTR, &[name, &[0]]).map(drop)?;
        self.stale();
        Ok(())
    }

    fn access_list(&self) -> Result<Option<KArc<acl::Acl>>> {
        // The program checks its own access lists, if it keeps any.
        Ok(None)
    }

    fn default_list(&self) -> Result<Option<KArc<acl::Acl>>> {
        Ok(None)
    }

    fn sync(&self) -> Result<()> {
        let Some(fh) = self.files.lock().write else { return Ok(()) };
        let mut body = [0u8; 16];
        body[..8].copy_from_slice(&fh.to_le_bytes());
        match self.call(op::FSYNC, &[&body]) {
            Ok(_) | Err(Error::NotSupported) => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
