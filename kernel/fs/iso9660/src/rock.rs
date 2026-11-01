// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Rock Ridge: POSIX files on ISO 9660, in the System Use area of each
//! directory record (the System Use Sharing Protocol, IEEE P1281, and the
//! Rock Ridge Interchange Protocol, IEEE P1282, 1.12).
//!
//! Each entry has a two-letter signature, a length and a version. The
//! root directory's own record starts with `SP`, which says SUSP is used
//! and how many bytes of each System Use area to skip. An area too small
//! for its entries goes on in a continuation area (`CE`) elsewhere on the
//! disc. Rock Ridge's entries: `PX` (mode, links, owner, group), `PN`
//! (device numbers), `SL` (a symbolic link's target, in components), `NM`
//! (the name, in pieces), `TF` (times), and `CL`, `PL` and `RE`, which
//! relocate directories deeper than ISO 9660's eight levels into a
//! directory of their own and leave a link where they belong.

use crate::format::{NameBuf, le32, long_date, short_date};

/// SUSP's indicator in the root's own record: `SP`, its check bytes, and
/// how many bytes of each System Use area come before its entries.
pub fn indicator(area: &[u8]) -> Option<u8> {
    (area.len() >= 7 && &area[..2] == b"SP" && area[2] >= 7 && area[4..6] == [0xbe, 0xef])
        .then(|| area[6])
}

/// A continuation area: where the System Use entries go on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Continuation {
    /// Its logical block.
    pub block: u32,
    /// Where in the block it begins, in bytes.
    pub offset: u32,
    /// Its length in bytes.
    pub len: u32,
}

/// Calls `f` with each entry of `area` (its signature and its bytes, the
/// header included) until `ST`, or until `f` returns false; returns the
/// continuation area it names, if any.
pub fn walk(area: &[u8], f: &mut dyn FnMut([u8; 2], &[u8]) -> bool) -> Option<Continuation> {
    let mut next = None;
    let mut at = 0;
    while at + 4 <= area.len() {
        let len = usize::from(area[at + 2]);
        if len < 4 || at + len > area.len() {
            break;
        }
        let entry = &area[at..at + len];
        let signature = [entry[0], entry[1]];
        match &signature {
            b"ST" => break,
            b"CE" if len >= 28 => {
                next = Some(Continuation {
                    block: le32(entry, 4),
                    offset: le32(entry, 12),
                    len: le32(entry, 20),
                });
            }
            _ => {
                if !f(signature, entry) {
                    break;
                }
            }
        }
        at += len;
    }
    next
}

/// The kinds of file a `PX` mode's type bits name.
pub mod mode {
    /// The type bits.
    pub const TYPE: u32 = 0o170_000;
    /// A socket.
    pub const SOCKET: u32 = 0o140_000;
    /// A symbolic link.
    pub const LINK: u32 = 0o120_000;
    /// A regular file.
    pub const FILE: u32 = 0o100_000;
    /// A block device.
    pub const BLOCK: u32 = 0o060_000;
    /// A directory.
    pub const DIRECTORY: u32 = 0o040_000;
    /// A character device.
    pub const CHAR: u32 = 0o020_000;
    /// A named pipe.
    pub const FIFO: u32 = 0o010_000;
}

/// What a record's Rock Ridge entries say of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attributes {
    /// `PX`: its mode (type and permissions).
    pub mode: Option<u32>,
    /// `PX`: how many names it has.
    pub links: u32,
    /// `PX`: the user it belongs to.
    pub owner: u32,
    /// `PX`: its group.
    pub group: u32,
    /// `PN`: the device a device file stands for.
    pub device: Option<u64>,
    /// `TF`: when it was made, in nanoseconds since 1970.
    pub created: Option<u64>,
    /// `TF`: when its contents last changed.
    pub modified: Option<u64>,
    /// `TF`: when it was last read.
    pub accessed: Option<u64>,
    /// `TF`: when its attributes last changed.
    pub changed: Option<u64>,
    /// `CL`: a relocated directory's block, where this record stands in
    /// for it.
    pub child: Option<u32>,
    /// `RE`: this is a relocated directory, listed where its `CL` is.
    pub relocated: bool,
    /// `SL`: it is a symbolic link.
    pub link: bool,
    /// `NM` said the name, whole.
    pub named: bool,
}

/// What [`Collect`] gathers of a record's entries.
pub struct Collect<'a> {
    /// What it says of the file.
    pub attributes: Attributes,
    /// Its name, from `NM`, if one is given.
    pub name: &'a mut NameBuf,
    /// Its symbolic link's target, from `SL`, if wanted.
    pub target: Option<&'a mut Target>,
    /// An `NM` with more to come.
    name_open: bool,
}

impl<'a> Collect<'a> {
    /// Gathers into `name`, and a link's target into `target` if given.
    pub fn new(name: &'a mut NameBuf, target: Option<&'a mut Target>) -> Collect<'a> {
        name.clear();
        Collect { attributes: Attributes::default(), name, target, name_open: false }
    }

    /// Takes one entry, as [`walk`] gives it.
    pub fn entry(&mut self, signature: [u8; 2], entry: &[u8]) {
        let a = &mut self.attributes;
        match &signature {
            b"PX" if entry.len() >= 36 => {
                a.mode = Some(le32(entry, 4));
                a.links = le32(entry, 12);
                a.owner = le32(entry, 20);
                a.group = le32(entry, 28);
            }
            b"PN" if entry.len() >= 20 => {
                let (high, low) = (u64::from(le32(entry, 4)), u64::from(le32(entry, 12)));
                // A 64-bit device number in two halves, or one in the low
                // half alone (as Linux's `old_encode_dev` writes it).
                a.device = Some(if high == 0 { low } else { (high << 32) | low });
            }
            b"TF" if entry.len() >= 5 => times(a, entry),
            b"CL" if entry.len() >= 12 => a.child = Some(le32(entry, 4)),
            b"RE" => a.relocated = true,
            b"NM" if entry.len() >= 5 => {
                let flags = entry[4];
                // The current and parent directories' own names are never
                // shown.
                if flags & 0b110 == 0 {
                    if !self.name_open {
                        self.name.clear();
                    }
                    self.name.push_bytes(&entry[5..]);
                    self.name_open = flags & 1 != 0;
                    a.named = true;
                }
            }
            b"SL" if entry.len() >= 5 => {
                a.link = true;
                if let Some(target) = self.target.as_deref_mut() {
                    target.components(&entry[5..]);
                }
            }
            _ => {}
        }
    }
}

/// Reads a `TF` entry's times.
fn times(a: &mut Attributes, entry: &[u8]) {
    let flags = entry[4];
    let long = flags & 0x80 != 0;
    let size = if long { 17 } else { 7 };
    let mut at = 5;
    let mut slots = [&mut a.created, &mut a.modified, &mut a.accessed, &mut a.changed];
    for (bit, slot) in slots.iter_mut().enumerate() {
        if flags & (1 << bit) == 0 {
            continue;
        }
        let Some(bytes) = entry.get(at..at + size) else { return };
        let time = if long {
            long_date(bytes.try_into().unwrap_or(&[0; 17]))
        } else {
            short_date(bytes.try_into().unwrap_or(&[0; 7]))
        };
        **slot = (time != 0).then_some(time);
        at += size;
    }
}

/// The most bytes of a symbolic link's target.
pub const TARGET_MAX: usize = 1024;

/// A symbolic link's target, built from `SL` entries' components.
pub struct Target {
    bytes: [u8; TARGET_MAX],
    len: usize,
    /// The last component went on in the next one (its `CONTINUE` bit).
    open: bool,
    /// Whether nothing has been added: the next component needs no `/`.
    first: bool,
    too_long: bool,
}

impl Default for Target {
    fn default() -> Self {
        Target { bytes: [0; TARGET_MAX], len: 0, open: false, first: true, too_long: false }
    }
}

impl Target {
    fn push(&mut self, bytes: &[u8]) {
        let end = self.len + bytes.len();
        if end > TARGET_MAX {
            self.too_long = true;
            return;
        }
        self.bytes[self.len..end].copy_from_slice(bytes);
        self.len = end;
    }

    /// Adds the components in an `SL` entry's bytes after its flags.
    fn components(&mut self, mut bytes: &[u8]) {
        while let [flags, len, rest @ ..] = bytes {
            let len = usize::from(*len);
            let Some(content) = rest.get(..len) else { return };
            if !self.open && !self.first && !matches!(self.bytes[..self.len], [.., b'/']) {
                self.push(b"/");
            }
            if flags & 0b1000 != 0 {
                // The root: the target is absolute.
                self.push(b"/");
            } else if flags & 0b10 != 0 {
                self.push(b".");
            } else if flags & 0b100 != 0 {
                self.push(b"..");
            } else {
                self.push(content);
            }
            self.first = false;
            self.open = flags & 1 != 0;
            bytes = &rest[len..];
        }
    }

    /// The target, or `None` if it was too long.
    pub fn bytes(&self) -> Option<&[u8]> {
        (!self.too_long).then_some(&self.bytes[..self.len])
    }
}
