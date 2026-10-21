// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The crash log: console output kept in RAM for the next boot.
//!
//! A machine without a serial port, such as the reference laptop, loses a
//! kernel's last words when it crashes. So the console output also goes to a
//! region of RAM that the architecture sets aside at the same physical
//! address on every boot. A warm restart keeps RAM contents on most
//! machines, and the next boot finds the log there and shows its end.
//!
//! The region is split into two slots, each a 64-byte header followed by a
//! ring of text. Each boot writes the slot the previous boot did not use,
//! so the previous log survives until the boot after next. A slot's header
//! records the boot number and how that boot ended. The format is
//! described in radix-debug(7).

use core::fmt;

/// First eight bytes of a valid slot.
pub const MAGIC: [u8; 8] = *b"RADIXLOG";
/// Format version.
pub const VERSION: u32 = 1;
/// Bytes of header at the start of each slot.
pub const HEADER: usize = 64;
/// Smallest region [`CrashLog::open`] accepts.
pub const MIN_REGION: usize = 2 * (HEADER + 1024);

/// How the boot that wrote a slot ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    /// Still running when the slot was last written: the machine hung, was
    /// reset, or lost power while running.
    Running,
    /// The kernel panicked.
    Panicked,
    /// The kernel restarted the machine on request.
    Restarted,
}

impl Ending {
    fn code(self) -> u32 {
        match self {
            Ending::Running => 1,
            Ending::Panicked => 2,
            Ending::Restarted => 3,
        }
    }

    fn from_code(code: u32) -> Option<Ending> {
        match code {
            1 => Some(Ending::Running),
            2 => Some(Ending::Panicked),
            3 => Some(Ending::Restarted),
            _ => None,
        }
    }
}

impl fmt::Display for Ending {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Ending::Running => "stopped while running (hang, reset or power loss)",
            Ending::Panicked => "ended with a kernel panic",
            Ending::Restarted => "ended with a requested restart",
        })
    }
}

/// Parsed header of a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Header {
    boot: u32,
    ending: Ending,
    written: u64,
}

const OFF_VERSION: usize = 8;
const OFF_BOOT: usize = 12;
const OFF_ENDING: usize = 16;
const OFF_RING_SIZE: usize = 20;
const OFF_WRITTEN: usize = 24;
const OFF_CHECKSUM: usize = 32;

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("4 bytes"))
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("8 bytes"))
}

/// FNV-1a over the header bytes before the checksum field.
fn checksum(header: &[u8]) -> u32 {
    header[..OFF_CHECKSUM]
        .iter()
        .fold(0x811c_9dc5u32, |hash, &byte| (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193))
}

fn parse_header(slot: &[u8]) -> Option<Header> {
    let header = &slot[..HEADER];
    if header[..8] != MAGIC
        || read_u32(header, OFF_VERSION) != VERSION
        || read_u32(header, OFF_RING_SIZE) as usize != slot.len() - HEADER
        || read_u32(header, OFF_CHECKSUM) != checksum(header)
    {
        return None;
    }
    Some(Header {
        boot: read_u32(header, OFF_BOOT),
        ending: Ending::from_code(read_u32(header, OFF_ENDING))?,
        written: read_u64(header, OFF_WRITTEN),
    })
}

fn write_header(slot: &mut [u8], header: Header) {
    let ring_size = (slot.len() - HEADER) as u32;
    let bytes = &mut slot[..HEADER];
    bytes.fill(0);
    bytes[..8].copy_from_slice(&MAGIC);
    bytes[OFF_VERSION..OFF_VERSION + 4].copy_from_slice(&VERSION.to_le_bytes());
    bytes[OFF_BOOT..OFF_BOOT + 4].copy_from_slice(&header.boot.to_le_bytes());
    bytes[OFF_ENDING..OFF_ENDING + 4].copy_from_slice(&header.ending.code().to_le_bytes());
    bytes[OFF_RING_SIZE..OFF_RING_SIZE + 4].copy_from_slice(&ring_size.to_le_bytes());
    bytes[OFF_WRITTEN..OFF_WRITTEN + 8].copy_from_slice(&header.written.to_le_bytes());
    let sum = checksum(bytes);
    bytes[OFF_CHECKSUM..OFF_CHECKSUM + 4].copy_from_slice(&sum.to_le_bytes());
}

/// The log of the boot before this one, read from its slot.
#[derive(Clone, Copy, Debug)]
pub struct Previous<'a> {
    /// That boot's number.
    pub boot: u32,
    /// How it ended.
    pub ending: Ending,
    /// Its text in order: `older` then `newer`.
    older: &'a [u8],
    newer: &'a [u8],
}

impl<'a> Previous<'a> {
    fn new(slot: &'a [u8], header: Header) -> Self {
        let ring = &slot[HEADER..];
        let size = ring.len() as u64;
        if header.written <= size {
            Previous {
                boot: header.boot,
                ending: header.ending,
                older: &ring[..header.written as usize],
                newer: &[],
            }
        } else {
            // The ring wrapped: the oldest byte is at the write position.
            let start = (header.written % size) as usize;
            Previous {
                boot: header.boot,
                ending: header.ending,
                older: &ring[start..],
                newer: &ring[..start],
            }
        }
    }

    /// Bytes of text kept.
    pub fn len(&self) -> usize {
        self.older.len() + self.newer.len()
    }

    /// Whether no text was kept.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The kept lines, oldest first. A line cut by the ring's wrap-around
    /// comes first, partial. Bytes that are not printable ASCII show as `?`.
    pub fn lines(&self) -> Lines<'a> {
        Lines { bytes: self.older.iter().chain(self.newer.iter()), done: false }
    }

    /// Number of lines kept.
    pub fn line_count(&self) -> usize {
        self.lines().count()
    }

    /// Whether some line contains `needle`.
    pub fn contains(&self, needle: &str) -> bool {
        self.lines().any(|line| line.as_str().contains(needle))
    }
}

/// One line of a [`Previous`] log.
#[derive(Clone)]
pub struct Line {
    bytes: [u8; Line::MAX],
    len: usize,
}

impl Line {
    /// Longest line kept; longer lines are split.
    pub const MAX: usize = 240;

    /// The line's text, without the newline.
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }
}

impl fmt::Display for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Iterator over the lines of a [`Previous`] log.
pub struct Lines<'a> {
    bytes: core::iter::Chain<core::slice::Iter<'a, u8>, core::slice::Iter<'a, u8>>,
    done: bool,
}

impl Iterator for Lines<'_> {
    type Item = Line;

    fn next(&mut self) -> Option<Line> {
        if self.done {
            return None;
        }
        let mut line = Line { bytes: [0; Line::MAX], len: 0 };
        loop {
            match self.bytes.next() {
                None => {
                    self.done = true;
                    return (line.len > 0).then_some(line);
                }
                Some(b'\n') => return Some(line),
                Some(b'\r') => {}
                Some(&byte) => {
                    let printable = byte == b'\t' || (0x20..0x7f).contains(&byte);
                    line.bytes[line.len] = if printable { byte } else { b'?' };
                    line.len += 1;
                    if line.len == Line::MAX {
                        return Some(line);
                    }
                }
            }
        }
    }
}

/// The crash log of the running boot.
pub struct CrashLog {
    region: &'static mut [u8],
    current: usize,
    header: Header,
    previous: Option<(usize, Header)>,
}

impl CrashLog {
    /// Takes over `region`: finds the previous boot's slot, if any, and
    /// starts this boot's log in the other one.
    ///
    /// Returns `None` if the region is smaller than [`MIN_REGION`].
    pub fn open(region: &'static mut [u8]) -> Option<CrashLog> {
        if region.len() < MIN_REGION {
            return None;
        }
        let half = region.len() / 2;
        let slots = [parse_header(&region[..half]), parse_header(&region[half..2 * half])];
        let previous = match slots {
            [Some(a), Some(b)] if b.boot > a.boot => Some((1, b)),
            [Some(a), _] => Some((0, a)),
            [None, Some(b)] => Some((1, b)),
            [None, None] => None,
        };
        let current = match previous {
            Some((0, _)) => 1,
            _ => 0,
        };
        let boot = previous.map_or(1, |(_, header)| header.boot.wrapping_add(1).max(1));
        let header = Header { boot, ending: Ending::Running, written: 0 };
        let mut log = CrashLog { region, current, header, previous };
        let slot = log.slot_mut(current);
        write_header(slot, header);
        Some(log)
    }

    fn half(&self) -> usize {
        self.region.len() / 2
    }

    fn slot(&self, index: usize) -> &[u8] {
        let half = self.half();
        &self.region[index * half..(index + 1) * half]
    }

    fn slot_mut(&mut self, index: usize) -> &mut [u8] {
        let half = self.half();
        &mut self.region[index * half..(index + 1) * half]
    }

    /// This boot's number: one more than the previous boot's.
    pub fn boot(&self) -> u32 {
        self.header.boot
    }

    /// The previous boot's log, if the region held one.
    pub fn previous(&self) -> Option<Previous<'_>> {
        let (index, header) = self.previous?;
        Some(Previous::new(self.slot(index), header))
    }

    /// Appends `text` to this boot's log. `flush` is called with every
    /// range of the region that changed, so the caller can write it back
    /// from the CPU caches to RAM.
    pub fn write(&mut self, text: &str, flush: &mut dyn FnMut(&[u8])) {
        let current = self.current;
        let half = self.half();
        let size = half - HEADER;
        let mut bytes = text.as_bytes();
        if bytes.len() > size {
            bytes = &bytes[bytes.len() - size..];
        }
        let mut position = (self.header.written % size as u64) as usize;
        let base = current * half + HEADER;
        while !bytes.is_empty() {
            let chunk = bytes.len().min(size - position);
            let range = base + position..base + position + chunk;
            self.region[range.clone()].copy_from_slice(&bytes[..chunk]);
            flush(&self.region[range]);
            bytes = &bytes[chunk..];
            position = (position + chunk) % size;
        }
        self.header.written += text.len().min(size) as u64;
        self.commit_header(flush);
    }

    /// Gives the region back, as the next boot would find it.
    pub fn into_region(self) -> &'static mut [u8] {
        self.region
    }

    /// Records how this boot ends.
    pub fn set_ending(&mut self, ending: Ending, flush: &mut dyn FnMut(&[u8])) {
        self.header.ending = ending;
        self.commit_header(flush);
    }

    fn commit_header(&mut self, flush: &mut dyn FnMut(&[u8])) {
        let header = self.header;
        let current = self.current;
        write_header(self.slot_mut(current), header);
        flush(&self.slot(current)[..HEADER]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(len: usize) -> &'static mut [u8] {
        Box::leak(vec![0u8; len].into_boxed_slice())
    }

    fn no_flush() -> impl FnMut(&[u8]) {
        |_| {}
    }

    #[test]
    fn first_boot_has_no_previous_log() {
        let log = CrashLog::open(region(4096)).unwrap();
        assert_eq!(log.boot(), 1);
        assert!(log.previous().is_none());
        assert!(CrashLog::open(region(100)).is_none());
    }

    #[test]
    fn survives_into_the_next_boot() {
        let mut flushed = 0;
        let mut log = CrashLog::open(region(4096)).unwrap();
        log.write("[    0.1] hello\n[    0.2] world\n", &mut |bytes| flushed += bytes.len());
        log.set_ending(Ending::Panicked, &mut no_flush());
        assert!(flushed > 0);

        // The next boot sees the same memory.
        let log = CrashLog::open(log.into_region()).unwrap();
        assert_eq!(log.boot(), 2);
        let previous = log.previous().unwrap();
        assert_eq!(previous.boot, 1);
        assert_eq!(previous.ending, Ending::Panicked);
        let lines: Vec<String> = previous.lines().map(|l| l.to_string()).collect();
        assert_eq!(lines, ["[    0.1] hello", "[    0.2] world"]);
        assert!(previous.contains("world"));
    }

    #[test]
    fn alternates_slots_and_keeps_the_newest() {
        let mut log = CrashLog::open(region(4096)).unwrap();
        log.write("boot one\n", &mut no_flush());
        let mut log = CrashLog::open(log.into_region()).unwrap();
        assert_eq!(log.previous().unwrap().ending, Ending::Running);
        log.write("boot two\n", &mut no_flush());
        log.set_ending(Ending::Restarted, &mut no_flush());
        let log = CrashLog::open(log.into_region()).unwrap();
        assert_eq!(log.boot(), 3);
        let previous = log.previous().unwrap();
        assert_eq!(previous.boot, 2);
        assert_eq!(previous.ending, Ending::Restarted);
        assert!(previous.contains("boot two"));
        assert!(!previous.contains("boot one"));
    }

    #[test]
    fn wraps_around_and_sanitises() {
        let mut log = CrashLog::open(region(MIN_REGION)).unwrap();
        for i in 0..200 {
            log.write(&format!("line {i}\n"), &mut no_flush());
        }
        log.write("bad \u{7}byte\n", &mut no_flush());
        let log = CrashLog::open(log.into_region()).unwrap();
        let previous = log.previous().unwrap();
        assert_eq!(previous.len(), MIN_REGION / 2 - HEADER);
        let lines: Vec<String> = previous.lines().map(|l| l.to_string()).collect();
        assert_eq!(lines.last().unwrap(), "bad ?byte");
        assert_eq!(lines[lines.len() - 2], "line 199");
        assert!(!lines.iter().any(|line| line == "line 0"));
    }

    #[test]
    fn rejects_corrupt_headers() {
        let mut log = CrashLog::open(region(4096)).unwrap();
        log.write("x\n", &mut no_flush());
        let memory = log.into_region();
        memory[OFF_BOOT] ^= 0xff;
        let log = CrashLog::open(memory).unwrap();
        assert!(log.previous().is_none());
        assert_eq!(log.boot(), 1);
    }
}
