// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host tests: a filesystem in memory, served from a thread as a program
//! would serve it, through a connection the tests mount.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::thread;

use radix_vfs::mem::KArc;
use radix_vfs::{Access, Error, FileSystem, Kind, NodeRef, Slice, SliceMut};

use crate::abi::{self, Attr, Entry, InHeader, OutHeader, Reader, Writer, errno, op};
use crate::{Caller, Connection, DeviceError, FuseFs, Hooks, Wake};

thread_local! {
    /// What the next waits of this thread end with once the flag is set,
    /// as a signal or a stop would.
    static CUT: Cell<Option<(Wake, &'static AtomicBool)>> = const { Cell::new(None) };
}

fn wait(_key: usize, ready: &dyn Fn() -> bool, signals: bool) -> Wake {
    loop {
        if ready() {
            return Wake::Ready;
        }
        if let Some((how, flag)) = CUT.with(Cell::get)
            && flag.load(Ordering::Acquire)
            && (how == Wake::Stop || signals)
        {
            return how;
        }
        thread::yield_now();
    }
}

static CLOCK: AtomicU64 = AtomicU64::new(1);

fn setup() {
    crate::set_hooks(Hooks {
        wait,
        wake: |_| {},
        caller: || Caller { uid: 1000, gid: 1000, pid: 42 },
        now: || CLOCK.fetch_add(1, Ordering::Relaxed),
    });
}

/// A node of the memory filesystem.
#[derive(Clone, Default)]
struct MemNode {
    mode: u32,
    data: Vec<u8>,
    children: BTreeMap<Vec<u8>, u64>,
    /// How many names the kernel holds of it.
    lookups: u64,
}

/// The memory filesystem, and what the tests look at.
#[derive(Default)]
struct Mem {
    nodes: HashMap<u64, MemNode>,
    next: u64,
    next_fh: u64,
    open: u64,
    /// The `WRITE` sizes seen.
    writes: Vec<u32>,
    /// The minor version to answer `INIT` with, and the most a write
    /// takes.
    minor: u32,
    max_write: u32,
    /// The requests seen, by operation.
    seen: Vec<u32>,
    /// Requests left unanswered, by number.
    held: Vec<u64>,
    hold: Option<u32>,
}

fn attr(id: u64, n: &MemNode) -> Attr {
    Attr {
        ino: id,
        size: n.data.len() as u64,
        mode: n.mode,
        nlink: 1,
        uid: 1000,
        gid: 1000,
        mtime: 1_700_000_000,
        mtimensec: 5,
        ..Attr::default()
    }
}

fn name(r: &mut Reader<'_>) -> Vec<u8> {
    let rest = r.rest();
    let end = rest.iter().position(|&b| b == 0).unwrap();
    let n = rest[..end].to_vec();
    r.take(end + 1).unwrap();
    n
}

impl Mem {
    fn new(minor: u32, max_write: u32) -> Mem {
        let mut m = Mem { next: 2, minor, max_write, ..Mem::default() };
        m.nodes.insert(
            abi::ROOT,
            MemNode { mode: abi::file_type::DIRECTORY | 0o755, ..MemNode::default() },
        );
        m
    }

    fn entry(&mut self, id: u64) -> Vec<u8> {
        let node = self.nodes.get_mut(&id).unwrap();
        node.lookups += 1;
        let e = Entry { nodeid: id, attr_valid: 1, attr: attr(id, node), ..Entry::default() };
        let mut w = Writer::with_capacity(128).unwrap();
        e.write(&mut w).unwrap();
        w.0.clone()
    }

    fn make(&mut self, parent: u64, name: Vec<u8>, mode: u32) -> Result<u64, i32> {
        let dir = self.nodes.get_mut(&parent).ok_or(errno::ENOENT)?;
        if dir.children.contains_key(&name) {
            return Err(errno::EEXIST);
        }
        let id = self.next;
        self.next += 1;
        dir.children.insert(name, id);
        self.nodes.insert(id, MemNode { mode, ..MemNode::default() });
        Ok(id)
    }

    /// Answers one request: the answer's data, or an error number; none
    /// for a request without an answer.
    fn answer(&mut self, h: &InHeader, body: &[u8]) -> Option<Result<Vec<u8>, i32>> {
        let mut r = Reader::new(body);
        let id = h.nodeid;
        Some(match h.opcode {
            op::INIT => {
                let mut w = Writer::with_capacity(64).unwrap();
                w.u32(7).unwrap().u32(self.minor).unwrap().u32(0).unwrap().u32(0).unwrap();
                w.bytes(&[0; 4]).unwrap().u32(self.max_write).unwrap();
                Ok(w.0.clone())
            }
            op::FORGET => {
                let n = r.u64().unwrap();
                let node = self.nodes.get_mut(&id).unwrap();
                assert!(node.lookups >= n, "forgot more names than were given");
                node.lookups -= n;
                return None;
            }
            op::INTERRUPT => return None,
            op::LOOKUP => {
                let n = name(&mut r);
                match self.nodes[&id].children.get(&n) {
                    Some(&child) => Ok(self.entry(child)),
                    None => Err(errno::ENOENT),
                }
            }
            op::GETATTR | op::SETATTR => {
                if h.opcode == op::SETATTR {
                    let valid = r.u32().unwrap();
                    r.take(12).unwrap();
                    let size = r.u64().unwrap();
                    if valid & abi::set::SIZE != 0 {
                        self.nodes.get_mut(&id).unwrap().data.resize(size as usize, 0);
                    }
                }
                let mut w = Writer::with_capacity(104).unwrap();
                w.u64(1).unwrap().u32(0).unwrap().u32(0).unwrap();
                attr(id, &self.nodes[&id]).write(&mut w).unwrap();
                Ok(w.0.clone())
            }
            op::MKDIR => {
                let mode = r.u32().unwrap();
                r.u32().unwrap();
                let n = name(&mut r);
                self.make(id, n, abi::file_type::DIRECTORY | mode).map(|c| self.entry(c))
            }
            op::CREATE | op::MKNOD => {
                let first = r.u32().unwrap();
                let second = r.u32().unwrap();
                r.take(8).unwrap();
                let mode = if h.opcode == op::CREATE { second } else { first };
                let n = name(&mut r);
                self.make(id, n, mode).map(|c| {
                    let mut e = self.entry(c);
                    if h.opcode == op::CREATE {
                        self.next_fh += 1;
                        self.open += 1;
                        e.extend_from_slice(&self.next_fh.to_le_bytes());
                        e.extend_from_slice(&[0; 8]);
                    }
                    e
                })
            }
            op::SYMLINK => {
                let n = name(&mut r);
                let target = name(&mut r);
                self.make(id, n, abi::file_type::LINK | 0o777).map(|c| {
                    self.nodes.get_mut(&c).unwrap().data = target;
                    self.entry(c)
                })
            }
            op::READLINK => Ok(self.nodes[&id].data.clone()),
            op::OPEN | op::OPENDIR => {
                self.next_fh += 1;
                self.open += 1;
                let mut a = self.next_fh.to_le_bytes().to_vec();
                a.extend_from_slice(&[0; 8]);
                Ok(a)
            }
            op::RELEASE | op::RELEASEDIR => {
                self.open -= 1;
                Ok(Vec::new())
            }
            op::READ => {
                r.u64().unwrap();
                let at = r.u64().unwrap() as usize;
                let size = r.u32().unwrap() as usize;
                let data = &self.nodes[&id].data;
                let start = at.min(data.len());
                Ok(data[start..(start + size).min(data.len())].to_vec())
            }
            op::WRITE => {
                r.u64().unwrap();
                let at = r.u64().unwrap() as usize;
                let size = r.u32().unwrap();
                r.take(20).unwrap();
                self.writes.push(size);
                let data = &mut self.nodes.get_mut(&id).unwrap().data;
                let bytes = r.rest();
                assert_eq!(bytes.len(), size as usize);
                if data.len() < at + bytes.len() {
                    data.resize(at + bytes.len(), 0);
                }
                data[at..at + bytes.len()].copy_from_slice(bytes);
                Ok([size.to_le_bytes(), [0; 4]].concat())
            }
            op::READDIR => {
                r.u64().unwrap();
                let at = r.u64().unwrap() as usize;
                let mut w = Writer::with_capacity(4096).unwrap();
                let children: Vec<(Vec<u8>, u64)> =
                    self.nodes[&id].children.iter().map(|(n, &c)| (n.clone(), c)).collect();
                for (i, (n, c)) in children.iter().enumerate().skip(at) {
                    let t = (self.nodes[c].mode & abi::file_type::MASK) >> 12;
                    w.u64(*c).unwrap().u64(i as u64 + 1).unwrap().u32(n.len() as u32).unwrap();
                    w.u32(t).unwrap().bytes(n).unwrap();
                    let pad = (24 + n.len()).next_multiple_of(8) - (24 + n.len());
                    w.bytes(&[0; 8][..pad]).unwrap();
                }
                Ok(w.0.clone())
            }
            op::UNLINK | op::RMDIR => {
                let n = name(&mut r);
                let Some(&child) = self.nodes[&id].children.get(&n) else {
                    return Some(Err(errno::ENOENT));
                };
                let is_dir =
                    self.nodes[&child].mode & abi::file_type::MASK == abi::file_type::DIRECTORY;
                if is_dir != (h.opcode == op::RMDIR) {
                    return Some(Err(if is_dir { errno::EISDIR } else { errno::ENOTDIR }));
                }
                self.nodes.get_mut(&id).unwrap().children.remove(&n);
                Ok(Vec::new())
            }
            op::RENAME => {
                let to_dir = r.u64().unwrap();
                let from = name(&mut r);
                let to = name(&mut r);
                let child = self.nodes.get_mut(&id).unwrap().children.remove(&from).unwrap();
                self.nodes.get_mut(&to_dir).unwrap().children.insert(to, child);
                Ok(Vec::new())
            }
            op::STATFS => {
                let mut w = Writer::with_capacity(80).unwrap();
                w.u64(100)
                    .unwrap()
                    .u64(60)
                    .unwrap()
                    .u64(50)
                    .unwrap()
                    .u64(10)
                    .unwrap()
                    .u64(5)
                    .unwrap();
                w.u32(4096).unwrap().u32(255).unwrap().u32(4096).unwrap().bytes(&[0; 28]).unwrap();
                Ok(w.0.clone())
            }
            _ => Err(errno::ENOSYS),
        })
    }
}

fn reply(conn: &Connection, unique: u64, answer: Result<Vec<u8>, i32>) {
    let (error, data) = match answer {
        Ok(data) => (0, data),
        Err(e) => (-e, Vec::new()),
    };
    let len = (abi::OUT_HEADER + data.len()) as u32;
    let mut w = Writer::with_capacity(len as usize).unwrap();
    OutHeader { len, error, unique }.write(&mut w).unwrap();
    w.bytes(&data).unwrap();
    assert_eq!(conn.write(&Slice(&w.0)), Ok(len as usize));
}

/// Serves `mem` on `conn` until the mount goes.
fn serve(conn: KArc<Connection>, mem: Arc<StdMutex<Mem>>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buf = vec![0u8; crate::max_request()];
        loop {
            let n = match conn.read(&mut SliceMut(&mut buf), false) {
                Ok(n) => n,
                Err(DeviceError::NoDevice) => return,
                Err(err) => panic!("read: {err:?}"),
            };
            let mut r = Reader::new(&buf[..n]);
            let h = InHeader::read(&mut r).unwrap();
            assert_eq!(h.len as usize, n);
            let mut m = mem.lock().unwrap();
            m.seen.push(h.opcode);
            if m.hold == Some(h.opcode) {
                m.held.push(h.unique);
                continue;
            }
            let answer = m.answer(&h, r.rest());
            drop(m);
            if let Some(answer) = answer {
                reply(&conn, h.unique, answer);
            }
        }
    })
}

fn mounted(
    minor: u32,
    max_write: u32,
) -> (KArc<FuseFs>, Arc<StdMutex<Mem>>, thread::JoinHandle<()>) {
    setup();
    let conn = Connection::new().unwrap();
    let fs = FuseFs::mount(&conn, false).unwrap();
    let mem = Arc::new(StdMutex::new(Mem::new(minor, max_write)));
    let server = serve(conn, mem.clone());
    (fs, mem, server)
}

const RW: Access = Access { owner: 1000, group: 1000, mode: 0o644 };

fn names(dir: &NodeRef) -> Vec<(String, Kind)> {
    let mut found = Vec::new();
    dir.list(0, &mut |_, name, kind| {
        found.push((name.to_string(), kind));
        true
    })
    .unwrap();
    found
}

#[test]
fn files_are_made_written_read_listed_and_removed() {
    let (fs, mem, server) = mounted(31, 1 << 20);
    let root = fs.root();
    assert_eq!(root.lookup("missing").err(), Some(Error::NotFound));
    let dir = root.create("docs", Kind::Directory, Access { mode: 0o755, ..RW }).unwrap();
    let file = dir.create("note", Kind::File, RW).unwrap();
    assert_eq!(file.write(0, &Slice(b"hello, ")).unwrap(), 7);
    assert_eq!(file.write(radix_vfs::APPEND, &Slice(b"world")).unwrap(), 5);
    let mut back = [0u8; 32];
    assert_eq!(file.read(0, &mut SliceMut(&mut back)).unwrap(), 12);
    assert_eq!(&back[..12], b"hello, world");
    let stat = file.stat();
    assert_eq!((stat.kind, stat.size, stat.modified), (Kind::File, 12, 1_700_000_000_000_000_005));
    assert_eq!(file.access(), RW);
    // The same node for the same file, while it lives.
    let again = dir.lookup("note").unwrap();
    assert!(KArc::ptr_eq(&again, &file));
    let link = dir.make_link("to-note", "note", RW).unwrap();
    let mut target = [0u8; 16];
    assert_eq!(link.read_link(&mut SliceMut(&mut target)).unwrap(), 4);
    assert_eq!(&target[..4], b"note");
    assert_eq!(
        names(&dir),
        vec![("note".to_string(), Kind::File), ("to-note".to_string(), Kind::Symlink)]
    );
    dir.rename("note", &*root, "moved").unwrap();
    assert!(dir.lookup("note").is_err());
    file.set_size(5).unwrap();
    assert_eq!(file.stat().size, 5);
    let info = fs.info();
    assert_eq!((info.block_size, info.blocks, info.free), (4096, 100, 50));
    root.remove("moved").unwrap();
    dir.remove("to-note").unwrap();
    // A directory goes with RMDIR, after UNLINK says it is one.
    root.remove("docs").unwrap();
    assert!(mem.lock().unwrap().seen.ends_with(&[op::UNLINK, op::RMDIR]));
    drop((file, again, link, dir));
    // Extended attributes: the program has none, and is asked once.
    assert_eq!(root.get_attribute(b"user.x").err(), Some(Error::NotSupported));
    assert_eq!(root.get_attribute(b"user.x").err(), Some(Error::NotSupported));
    assert_eq!(mem.lock().unwrap().seen.iter().filter(|&&o| o == op::GETXATTR).count(), 1);
    drop(root);
    drop(fs);
    server.join().unwrap();
    let m = mem.lock().unwrap();
    // Every name given back, every file closed.
    assert!(m.nodes.values().all(|n| n.lookups == 0), "names not forgotten");
    assert_eq!(m.open, 0);
}

#[test]
fn writes_are_split_as_init_says() {
    // A program of 7.19, as fuser is by default: no pages in its answer,
    // so 32 pages a request at most, and its own 16 KiB.
    let (fs, mem, server) = mounted(19, 16 * 1024);
    let file = fs.root().create("big", Kind::File, RW).unwrap();
    let data = vec![7u8; 40 * 1024];
    assert_eq!(file.write(0, &Slice(&data)).unwrap(), data.len());
    assert_eq!(mem.lock().unwrap().writes, vec![16384, 16384, 8192]);
    let mut back = vec![0u8; 50 * 1024];
    assert_eq!(file.read(0, &mut SliceMut(&mut back)).unwrap(), data.len());
    assert_eq!(&back[..data.len()], &data[..]);
    drop(file);
    drop(fs);
    server.join().unwrap();
}

#[test]
fn an_older_protocol_is_refused() {
    let (fs, _mem, server) = mounted(11, 4096);
    assert_eq!(fs.root().lookup("x").err(), Some(Error::NotConnected));
    drop(fs);
    server.join().unwrap();
}

#[test]
fn a_lost_program_fails_the_calls() {
    setup();
    let conn = Connection::new().unwrap();
    let fs = FuseFs::mount(&conn, false).unwrap();
    // A second mount of the same connection is refused.
    assert_eq!(FuseFs::mount(&conn, false).err().map(|_| ()), Some(()));
    let mem = Arc::new(StdMutex::new(Mem::new(31, 4096)));
    mem.lock().unwrap().hold = Some(op::LOOKUP);
    let server = serve(conn.clone(), mem.clone());
    let root = fs.root();
    let asker = thread::spawn(move || root.lookup("x").err());
    while mem.lock().unwrap().held.is_empty() {
        thread::yield_now();
    }
    conn.lose();
    assert_eq!(asker.join().unwrap(), Some(Error::NotConnected));
    assert_eq!(fs.root().lookup("y").err(), Some(Error::NotConnected));
    server.join().unwrap();
}

#[test]
fn a_signal_sends_interrupt_once_the_request_is_read() {
    setup();
    static GATE: AtomicBool = AtomicBool::new(true);
    let conn = Connection::new().unwrap();
    let fs = FuseFs::mount(&conn, false).unwrap();
    // INIT answered, then nothing read.
    let mut buf = vec![0u8; crate::max_request()];
    let n = conn.read(&mut SliceMut(&mut buf), true).unwrap();
    let h = InHeader::read(&mut Reader::new(&buf[..n])).unwrap();
    let mut mem = Mem::new(31, 4096);
    let answer = mem.answer(&h, &buf[abi::IN_HEADER..n]).unwrap();
    reply(&conn, h.unique, answer);
    // A call with a signal for its thread: it waits on.
    let root = fs.root();
    let asker = thread::spawn(move || {
        CUT.with(|c| c.set(Some((Wake::Signal, &GATE))));
        root.lookup("x").err()
    });
    // Its request, then INTERRUPT for it, which the program answers by
    // stopping the call.
    let n = loop {
        match conn.read(&mut SliceMut(&mut buf), true) {
            Ok(n) => break n,
            Err(DeviceError::WouldBlock) => thread::yield_now(),
            Err(err) => panic!("{err:?}"),
        }
    };
    let call = InHeader::read(&mut Reader::new(&buf[..n])).unwrap();
    assert_eq!(call.opcode, op::LOOKUP);
    let n = loop {
        match conn.read(&mut SliceMut(&mut buf), true) {
            Ok(n) => break n,
            Err(DeviceError::WouldBlock) => thread::yield_now(),
            Err(err) => panic!("{err:?}"),
        }
    };
    let mut r = Reader::new(&buf[..n]);
    let interrupt = InHeader::read(&mut r).unwrap();
    assert_eq!(interrupt.opcode, op::INTERRUPT);
    assert_eq!(r.u64().unwrap(), call.unique);
    assert!(!asker.is_finished());
    reply(&conn, call.unique, Err(errno::EINTR));
    assert_eq!(asker.join().unwrap(), Some(Error::Interrupted));
    assert_eq!(conn.read(&mut SliceMut(&mut buf), true), Err(DeviceError::WouldBlock));
    drop(fs);
    assert_eq!(conn.read(&mut SliceMut(&mut buf), true), Err(DeviceError::NoDevice));
}

#[test]
fn a_stopped_call_forgets_the_node_it_was_given() {
    let (fs, mem, server) = mounted(31, 4096);
    static READ: AtomicBool = AtomicBool::new(false);
    fs.root().create("kept", Kind::File, RW).map(drop).unwrap();
    mem.lock().unwrap().hold = Some(op::LOOKUP);
    let root = fs.root();
    let asker = thread::spawn(move || {
        CUT.with(|c| c.set(Some((Wake::Stop, &READ))));
        root.lookup("kept").err()
    });
    while mem.lock().unwrap().held.is_empty() {
        thread::yield_now();
    }
    READ.store(true, Ordering::Release);
    assert_eq!(asker.join().unwrap(), Some(Error::Interrupted));
    // The program answers late: the kernel gives the name straight back.
    let unique = {
        let mut m = mem.lock().unwrap();
        m.hold = None;
        let unique = m.held.pop().unwrap();
        let child = m.nodes[&abi::ROOT].children[b"kept".as_slice()];
        Entry::read(&mut Reader::new(&m.entry(child))).unwrap();
        (unique, child)
    };
    let mut m = mem.lock().unwrap();
    let e = {
        let node = &m.nodes[&unique.1];
        let mut w = Writer::with_capacity(128).unwrap();
        Entry { nodeid: unique.1, attr: attr(unique.1, node), ..Entry::default() }
            .write(&mut w)
            .unwrap();
        w.0.clone()
    };
    drop(m);
    reply(fs.connection(), unique.0, Ok(e));
    // Its FORGET comes, then the mount goes.
    while mem.lock().unwrap().nodes[&unique.1].lookups != 0 {
        thread::yield_now();
    }
    m = mem.lock().unwrap();
    assert!(m.seen.contains(&op::FORGET));
    drop(m);
    drop(fs);
    server.join().unwrap();
}
