// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The zip a `.dkv` file is (ADR 0222): written plainly, and read as
//! something anyone could have made.
//!
//! Writing: each file stored or deflated, whichever is smaller, with its
//! Unix mode, the time 1980-01-01 00:00 and no extra fields, in the order
//! given, so that the same folder packs to the same bytes.
//!
//! Reading takes the central directory as what the archive holds and
//! checks it against each local header. It refuses what a bundle never
//! needs and attacks use: encryption, methods other than stored and
//! deflate, zip64, entries that overlap or hide between others, paths
//! that leave the folder, links, device files, pipes and sockets, set-ID
//! bits, a name twice, and more unpacked bytes than [`RATIO`] times the
//! archive. Each file's bytes are checked against its size and CRC-32 as
//! they are unpacked, and unpacking stops at the size the archive says.

use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::FileExt;

use miniz_oxide::inflate::stream::{InflateState, inflate};
use miniz_oxide::{DataFormat, MZError, MZFlush, MZStatus};

use super::sums;

const LOCAL: u32 = 0x0403_4b50;
const CENTRAL: u32 = 0x0201_4b50;
const END: u32 = 0x0605_4b50;
/// Version 2.0, the first with deflate and folders, made on Unix.
const NEEDED: u16 = 20;
const MADE_BY: u16 = 3 << 8 | NEEDED;
/// Bit 11: the name is UTF-8.
const UTF8: u16 = 1 << 11;
const STORED: u16 = 0;
const DEFLATED: u16 = 8;
/// 1980-01-01 00:00, the earliest time a zip holds.
const DOS_DATE: u16 = 1 << 5 | 1;
/// The most an archive's unpacked files may take, against its own size:
/// text and programs deflate to a third or a tenth, and a zip bomb to a
/// thousandth.
pub const RATIO: u64 = 100;
/// The least an archive may always unpack to, whatever its size.
const RATIO_FLOOR: u64 = 16 << 20;
/// The most files an archive without zip64 holds.
pub const ENTRIES_MAX: usize = 0xffff;

const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;

/// The CRC-32 of zip and gzip (IEEE 802.3), as bytes come.
#[derive(Clone, Copy)]
pub struct Crc(u32);

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = crc_table();

impl Crc {
    pub fn new() -> Crc {
        Crc(!0)
    }

    pub fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = CRC_TABLE[((self.0 ^ u32::from(b)) & 0xff) as usize] ^ (self.0 >> 8);
        }
    }

    pub fn value(self) -> u32 {
        !self.0
    }
}

/// A file to write into an archive.
pub struct Input<'a> {
    pub path: &'a str,
    pub executable: bool,
    pub data: &'a [u8],
}

/// Writes archives a file at a time.
pub struct Writer<W: Write> {
    out: W,
    offset: u64,
    central: Vec<u8>,
    entries: usize,
}

fn too_big() -> io::Error {
    io::Error::other("the bundle is bigger than a zip without zip64 holds: 4 GiB, 65535 files")
}

fn u32_of(value: u64) -> io::Result<u32> {
    u32::try_from(value).ok().filter(|&v| v != u32::MAX).ok_or_else(too_big)
}

impl<W: Write> Writer<W> {
    pub fn new(out: W) -> Writer<W> {
        Writer { out, offset: 0, central: Vec::new(), entries: 0 }
    }

    /// Adds `input`, deflated if that makes it smaller.
    pub fn add(&mut self, input: &Input<'_>) -> io::Result<()> {
        if self.entries == ENTRIES_MAX {
            return Err(too_big());
        }
        let deflated = miniz_oxide::deflate::compress_to_vec(input.data, 6);
        let (method, stored) = if deflated.len() < input.data.len() {
            (DEFLATED, &deflated[..])
        } else {
            (STORED, input.data)
        };
        let mut crc = Crc::new();
        crc.update(input.data);
        let crc = crc.value();
        let name = input.path.as_bytes();
        let name_len = u16::try_from(name.len()).map_err(|_| too_big())?;
        let size = u32_of(input.data.len() as u64)?;
        let packed = u32_of(stored.len() as u64)?;
        let offset = u32_of(self.offset)?;
        let mut local = Vec::with_capacity(30 + name.len());
        for field in [&LOCAL.to_le_bytes()[..], &NEEDED.to_le_bytes(), &UTF8.to_le_bytes()] {
            local.extend_from_slice(field);
        }
        let common = [
            &method.to_le_bytes()[..],
            &0u16.to_le_bytes(),
            &DOS_DATE.to_le_bytes(),
            &crc.to_le_bytes(),
            &packed.to_le_bytes(),
            &size.to_le_bytes(),
            &name_len.to_le_bytes(),
            &0u16.to_le_bytes(),
        ];
        for field in common {
            local.extend_from_slice(field);
        }
        local.extend_from_slice(name);
        self.out.write_all(&local)?;
        self.out.write_all(stored)?;
        let mode: u32 = S_IFREG | if input.executable { 0o755 } else { 0o644 };
        for field in [&CENTRAL.to_le_bytes()[..], &MADE_BY.to_le_bytes(), &NEEDED.to_le_bytes()] {
            self.central.extend_from_slice(field);
        }
        self.central.extend_from_slice(&UTF8.to_le_bytes());
        for field in common {
            self.central.extend_from_slice(field);
        }
        for field in [
            &0u16.to_le_bytes()[..],
            &0u16.to_le_bytes(),
            &0u16.to_le_bytes(),
            &(mode << 16).to_le_bytes(),
            &offset.to_le_bytes(),
        ] {
            self.central.extend_from_slice(field);
        }
        self.central.extend_from_slice(name);
        self.offset += (local.len() + stored.len()) as u64;
        self.entries += 1;
        Ok(())
    }

    /// Writes the central directory and the end record.
    pub fn finish(mut self) -> io::Result<W> {
        let offset = u32_of(self.offset)?;
        let size = u32_of(self.central.len() as u64)?;
        let count = u16::try_from(self.entries).map_err(|_| too_big())?;
        u32_of(self.offset + self.central.len() as u64 + 22)?;
        self.out.write_all(&self.central)?;
        let mut end = Vec::with_capacity(22);
        for field in [
            &END.to_le_bytes()[..],
            &0u16.to_le_bytes(),
            &0u16.to_le_bytes(),
            &count.to_le_bytes(),
            &count.to_le_bytes(),
            &size.to_le_bytes(),
            &offset.to_le_bytes(),
            &0u16.to_le_bytes(),
        ] {
            end.extend_from_slice(field);
        }
        self.out.write_all(&end)?;
        Ok(self.out)
    }
}

/// A file or folder an archive holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Its path, without the `/` a folder's ends in.
    pub path: String,
    pub folder: bool,
    pub executable: bool,
    pub method: u16,
    pub crc: u32,
    pub packed: u64,
    pub size: u64,
    /// Where its local header starts, and where its bytes do.
    header: u64,
    data: u64,
}

/// Where an archive's bytes come from: a file, or bytes in memory.
pub trait Source {
    fn len(&self) -> io::Result<u64>;
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;

    /// Fills `buf` from `offset`, or fails.
    fn read_exact_at(&self, buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        let mut done = 0;
        while done < buf.len() {
            match self.read_at(&mut buf[done..], offset)? {
                0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                n => {
                    done += n;
                    offset += n as u64;
                }
            }
        }
        Ok(())
    }
}

impl Source for File {
    fn len(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        FileExt::read_at(self, buf, offset)
    }
}

impl Source for [u8] {
    fn len(&self) -> io::Result<u64> {
        Ok(<[u8]>::len(self) as u64)
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let start = usize::try_from(offset).unwrap_or(usize::MAX).min(<[u8]>::len(self));
        let n = buf.len().min(<[u8]>::len(self) - start);
        buf[..n].copy_from_slice(&self[start..start + n]);
        Ok(n)
    }
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// An archive's directory, read and checked.
pub struct Archive<'a, S: Source + ?Sized> {
    source: &'a S,
    pub entries: Vec<Entry>,
}

#[allow(clippy::needless_pass_by_value)] // As `map_err` passes it.
fn io_error(e: io::Error) -> String {
    format!("cannot read it: {e}")
}

impl<'a, S: Source + ?Sized> Archive<'a, S> {
    /// Reads and checks the archive's directory.
    pub fn read(source: &'a S) -> Result<Archive<'a, S>, String> {
        let size = source.len().map_err(io_error)?;
        if size < 22 {
            return Err("not a zip: it is too short".into());
        }
        if size >= u64::from(u32::MAX) {
            return Err("bigger than a zip without zip64 holds, 4 GiB".into());
        }
        // The end record is in the last 22 bytes and its comment.
        let tail_len = size.min(22 + 0xffff);
        let mut tail = vec![0u8; tail_len as usize];
        source.read_exact_at(&mut tail, size - tail_len).map_err(io_error)?;
        let at = (0..=tail.len() - 22)
            .rev()
            .find(|&i| {
                u32_at(&tail, i) == END && i + 22 + usize::from(u16_at(&tail, i + 20)) == tail.len()
            })
            .ok_or("not a zip: it has no end record")?;
        let end = &tail[at..at + 22];
        let end_at = size - tail_len + at as u64;
        let (disk, cd_disk, here, count) =
            (u16_at(end, 4), u16_at(end, 6), u16_at(end, 8), u16_at(end, 10));
        let (cd_size, cd_offset) = (u64::from(u32_at(end, 12)), u64::from(u32_at(end, 16)));
        if count == 0xffff || cd_size == 0xffff_ffff || cd_offset == 0xffff_ffff {
            return Err("a zip64 archive, which a bundle is not".into());
        }
        if disk != 0 || cd_disk != 0 || here != count {
            return Err("an archive split over several disks, which a bundle is not".into());
        }
        if cd_offset + cd_size != end_at {
            return Err("damaged: its directory is not where its end record says".into());
        }
        let mut directory = vec![0u8; cd_size as usize];
        source.read_exact_at(&mut directory, cd_offset).map_err(io_error)?;
        let mut entries: Vec<Entry> = Vec::with_capacity(usize::from(count));
        let mut at = 0usize;
        for _ in 0..count {
            let (entry, next) = central_entry(&directory, at)?;
            at = next;
            entries.push(entry);
        }
        if at != directory.len() {
            return Err("damaged: its directory has more than its entries".into());
        }
        let mut archive = Archive { source, entries };
        archive.check_locals(cd_offset)?;
        archive.check_names()?;
        let total: u64 = archive.entries.iter().map(|e| e.size).sum();
        let most = (size * RATIO).max(RATIO_FLOOR);
        if total > most {
            return Err(format!(
                "its files unpack to {total} bytes, more than {RATIO} times its own size: \
                 an archive made to fill the disk"
            ));
        }
        Ok(archive)
    }

    /// Checks each entry's local header against the directory, and that no
    /// two entries' bytes overlap or leave a gap another could hide in.
    fn check_locals(&mut self, cd_offset: u64) -> Result<(), String> {
        let mut order: Vec<usize> = (0..self.entries.len()).collect();
        order.sort_by_key(|&i| self.entries[i].header);
        let mut next = 0u64;
        for i in order {
            let entry = &mut self.entries[i];
            if entry.header != next {
                return Err(format!(
                    "{}: its bytes overlap another file's, or leave a gap before it",
                    entry.path
                ));
            }
            let mut header = [0u8; 30];
            self.source.read_exact_at(&mut header, entry.header).map_err(io_error)?;
            let name_len = u64::from(u16_at(&header, 26));
            let extra_len = u64::from(u16_at(&header, 28));
            if u32_at(&header, 0) != LOCAL || u16_at(&header, 8) != entry.method {
                return Err(format!("{}: damaged: its header does not match", entry.path));
            }
            let mut name = vec![0u8; name_len as usize];
            self.source.read_exact_at(&mut name, entry.header + 30).map_err(io_error)?;
            let expected =
                if entry.folder { format!("{}/", entry.path) } else { entry.path.clone() };
            if name != expected.as_bytes() {
                return Err(format!("{}: damaged: its header names another file", entry.path));
            }
            entry.data = entry.header + 30 + name_len + extra_len;
            next = entry.data + entry.packed;
            // A data descriptor may follow, as streaming writers leave one.
            if u16_at(&header, 6) & 1 << 3 != 0 {
                next = descriptor_end(self.source, entry, next)?;
            }
            if next > cd_offset {
                return Err(format!("{}: damaged: its bytes run into the directory", entry.path));
            }
        }
        if next != cd_offset {
            return Err("damaged: bytes between its last file and its directory".into());
        }
        Ok(())
    }

    /// Checks the paths: each once, none a file and a folder at once.
    fn check_names(&self) -> Result<(), String> {
        let mut paths: Vec<(&str, bool)> =
            self.entries.iter().map(|e| (e.path.as_str(), e.folder)).collect();
        paths.sort_unstable();
        for pair in paths.windows(2) {
            if pair[0].0 == pair[1].0 {
                return Err(format!("{}: in the archive twice", pair[0].0));
            }
        }
        for (path, folder) in &paths {
            if *folder {
                continue;
            }
            let inside = format!("{path}/");
            if paths.iter().any(|(other, _)| other.starts_with(&inside)) {
                return Err(format!("{path}: a file, and a folder of others"));
            }
        }
        Ok(())
    }

    /// The entry at `path`, if the archive holds that file.
    pub fn file(&self, path: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| !e.folder && e.path == path)
    }

    /// Unpacks `entry`, handing its bytes to `out` as they come, and checks
    /// its size and CRC-32. It stops at the size the directory says.
    pub fn unpack(
        &self,
        entry: &Entry,
        out: &mut dyn FnMut(&[u8]) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut crc = Crc::new();
        let mut written = 0u64;
        let mut sink = |bytes: &[u8]| -> Result<(), String> {
            written += bytes.len() as u64;
            if written > entry.size {
                return Err(format!("{}: damaged: more bytes than it says", entry.path));
            }
            crc.update(bytes);
            out(bytes)
        };
        let mut input = vec![0u8; 64 << 10];
        let mut offset = 0u64;
        if entry.method == STORED {
            while offset < entry.packed {
                let n = input.len().min((entry.packed - offset) as usize);
                self.source
                    .read_exact_at(&mut input[..n], entry.data + offset)
                    .map_err(io_error)?;
                sink(&input[..n])?;
                offset += n as u64;
            }
        } else {
            let mut state = InflateState::new_boxed(DataFormat::Raw);
            let mut output = vec![0u8; 64 << 10];
            let mut have = 0usize;
            let mut start = 0usize;
            loop {
                if start == have && offset < entry.packed {
                    have = input.len().min((entry.packed - offset) as usize);
                    self.source
                        .read_exact_at(&mut input[..have], entry.data + offset)
                        .map_err(io_error)?;
                    offset += have as u64;
                    start = 0;
                }
                let flush = if offset == entry.packed { MZFlush::Finish } else { MZFlush::None };
                let result = inflate(&mut state, &input[start..have], &mut output, flush);
                start += result.bytes_consumed;
                sink(&output[..result.bytes_written])?;
                match result.status {
                    Ok(MZStatus::StreamEnd) => break,
                    Ok(_) => {}
                    Err(MZError::Buf) if offset == entry.packed && start == have => {
                        return Err(format!("{}: damaged: its bytes end too soon", entry.path));
                    }
                    Err(MZError::Buf) => {}
                    Err(_) => return Err(format!("{}: damaged: it does not inflate", entry.path)),
                }
                if result.bytes_consumed == 0
                    && result.bytes_written == 0
                    && offset == entry.packed
                    && start == have
                {
                    return Err(format!("{}: damaged: its bytes end too soon", entry.path));
                }
            }
            if start != have || offset != entry.packed {
                return Err(format!("{}: damaged: bytes after its end", entry.path));
            }
        }
        if written != entry.size {
            return Err(format!("{}: damaged: fewer bytes than it says", entry.path));
        }
        if crc.value() != entry.crc {
            return Err(format!("{}: damaged: its CRC-32 does not match", entry.path));
        }
        Ok(())
    }

    /// Unpacks a small file, such as the list or the signature, whole.
    pub fn read_small(&self, entry: &Entry, max: u64) -> Result<Vec<u8>, String> {
        if entry.size > max {
            return Err(format!("{}: bigger than it may be, {max} bytes", entry.path));
        }
        let mut bytes = Vec::with_capacity(entry.size as usize);
        self.unpack(entry, &mut |chunk| {
            bytes.extend_from_slice(chunk);
            Ok(())
        })?;
        Ok(bytes)
    }
}

/// Where the data descriptor after `entry`'s bytes, at `at`, ends: 12 or
/// 16 bytes, with or without its signature.
fn descriptor_end<S: Source + ?Sized>(source: &S, entry: &Entry, at: u64) -> Result<u64, String> {
    let mut bytes = [0u8; 16];
    let _ = source.read_at(&mut bytes, at).map_err(io_error)?;
    let fields = if u32_at(&bytes, 0) == 0x0807_4b50 { 4 } else { 0 };
    if u32_at(&bytes, fields) != entry.crc {
        return Err(format!("{}: damaged: its data descriptor does not match", entry.path));
    }
    Ok(at + fields as u64 + 12)
}

/// Reads and checks the directory entry at `at`; returns it and where the
/// next one starts.
fn central_entry(directory: &[u8], at: usize) -> Result<(Entry, usize), String> {
    let damaged = || "damaged: its directory is cut short".to_string();
    let fixed = directory.get(at..at + 46).ok_or_else(damaged)?;
    if u32_at(fixed, 0) != CENTRAL {
        return Err("damaged: its directory has something that is not an entry".into());
    }
    let made_by = u16_at(fixed, 4);
    let flags = u16_at(fixed, 8);
    let method = u16_at(fixed, 10);
    let crc = u32_at(fixed, 16);
    let packed = u32_at(fixed, 20);
    let size = u32_at(fixed, 24);
    let (name_len, extra_len, comment_len) = (
        usize::from(u16_at(fixed, 28)),
        usize::from(u16_at(fixed, 30)),
        usize::from(u16_at(fixed, 32)),
    );
    let attributes = u32_at(fixed, 38);
    let header = u32_at(fixed, 42);
    let next = at + 46 + name_len + extra_len + comment_len;
    let name = directory.get(at + 46..at + 46 + name_len).ok_or_else(damaged)?;
    directory.get(at..next).ok_or_else(damaged)?;
    let name = std::str::from_utf8(name).map_err(|_| {
        format!("`{}`: a name that is not UTF-8", String::from_utf8_lossy(name).escape_debug())
    })?;
    let shown = name.escape_debug().to_string();
    if packed == u32::MAX || size == u32::MAX || header == u32::MAX {
        return Err(format!("{shown}: a zip64 entry, which a bundle does not have"));
    }
    if flags & (1 | 1 << 6 | 1 << 13) != 0 {
        return Err(format!("{shown}: encrypted, which a bundle is not"));
    }
    if method != STORED && method != DEFLATED {
        return Err(format!("{shown}: packed a way other than stored or deflated"));
    }
    let folder = name.ends_with('/');
    let path = name.strip_suffix('/').unwrap_or(name);
    sums::check_path(path).map_err(|why| format!("{shown}: {why}"))?;
    let unix = made_by >> 8 == 3;
    let mode = if unix { attributes >> 16 } else { 0 };
    match mode & S_IFMT {
        0 => {}
        S_IFREG if !folder => {}
        S_IFDIR if folder => {}
        S_IFLNK => return Err(format!("{path}: a symbolic link, which a bundle may not hold")),
        S_IFREG | S_IFDIR => return Err(format!("{path}: a file and a folder at once")),
        _ => {
            return Err(format!(
                "{path}: a device file, pipe or socket, which a bundle may not hold"
            ));
        }
    }
    if mode & 0o6000 != 0 {
        return Err(format!("{path}: it has set-ID bits, which a bundle's files may not have"));
    }
    // MS-DOS's folder bit, from archives made elsewhere.
    if !unix && attributes & 0x10 != 0 && !folder {
        return Err(format!("{path}: a folder whose name does not end in /"));
    }
    if method == STORED && packed != size {
        return Err(format!("{path}: damaged: stored, but its sizes differ"));
    }
    if folder && (size != 0 || packed != 0) {
        return Err(format!("{path}: a folder with bytes in it"));
    }
    let entry = Entry {
        path: path.into(),
        folder,
        executable: mode & 0o111 != 0,
        method,
        crc,
        packed: u64::from(packed),
        size: u64::from(size),
        header: u64::from(header),
        data: 0,
    };
    Ok((entry, next))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(files: &[(&str, bool, &[u8])]) -> Vec<u8> {
        let mut writer = Writer::new(Vec::new());
        for (path, executable, data) in files {
            writer.add(&Input { path, executable: *executable, data }).unwrap();
        }
        writer.finish().unwrap()
    }

    fn unpacked(archive: &Archive<'_, [u8]>, path: &str) -> Vec<u8> {
        archive.read_small(archive.file(path).unwrap(), u64::MAX).unwrap()
    }

    #[test]
    fn crc_32_is_zip_s() {
        let mut crc = Crc::new();
        crc.update(b"123456789");
        assert_eq!(crc.value(), 0xcbf4_3926);
    }

    #[test]
    fn archives_read_back_what_was_written() {
        let text = b"hello hello hello hello hello hello hello hello\n".repeat(100);
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let random: Vec<u8> = (0..5000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 32) as u8
            })
            .collect();
        let bytes = pack(&[
            ("manifest.json", false, b"{}\n"),
            ("bin/app", true, &random),
            ("share/text", false, &text),
            ("empty", false, b""),
        ]);
        let archive = Archive::read(&bytes[..]).unwrap();
        assert_eq!(archive.entries.len(), 4);
        assert_eq!(unpacked(&archive, "manifest.json"), b"{}\n");
        assert_eq!(unpacked(&archive, "bin/app"), random);
        assert_eq!(unpacked(&archive, "share/text"), text);
        assert_eq!(unpacked(&archive, "empty"), b"");
        let app = archive.file("bin/app").unwrap();
        assert!(app.executable && app.method == STORED);
        let shared = archive.file("share/text").unwrap();
        assert!(!shared.executable && shared.method == DEFLATED && shared.packed < 200);
        // The same files give the same bytes.
        let again = pack(&[
            ("manifest.json", false, b"{}\n"),
            ("bin/app", true, &random),
            ("share/text", false, &text),
            ("empty", false, b""),
        ]);
        assert_eq!(again, bytes);
    }

    /// An archive Info-ZIP's zip 3.0 made of a folder (`zip -r -X`), with a
    /// folder entry, read as it is.
    #[test]
    fn reads_what_info_zip_makes() {
        for bytes in [INFO_ZIP, STREAMED] {
            let archive = Archive::read(bytes).unwrap();
            let names: Vec<(&str, bool)> =
                archive.entries.iter().map(|e| (e.path.as_str(), e.folder)).collect();
            assert_eq!(names, [("b", true), ("b/hello.txt", false)]);
            assert_eq!(unpacked(&archive, "b/hello.txt"), b"hello\n");
        }
        assert_eq!(Archive::read(STREAMED).unwrap().entries[1].method, DEFLATED);
    }

    #[rustfmt::skip]
    const INFO_ZIP: &[u8] = &[
        80, 75, 3, 4, 10, 0, 0, 0, 0, 0, 0, 96, 95, 93, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 98, 47, 80, 75, 3, 4, 10, 0, 0, 0,
        0, 0, 0, 96, 95, 93, 32, 48, 58, 54, 6, 0, 0, 0, 6, 0, 0, 0, 11, 0,
        0, 0, 98, 47, 104, 101, 108, 108, 111, 46, 116, 120, 116, 104, 101, 108, 108, 111, 10, 80,
        75, 1, 2, 30, 3, 10, 0, 0, 0, 0, 0, 0, 96, 95, 93, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 16, 0, 237,
        65, 0, 0, 0, 0, 98, 47, 80, 75, 1, 2, 30, 3, 10, 0, 0, 0, 0, 0, 0,
        96, 95, 93, 32, 48, 58, 54, 6, 0, 0, 0, 6, 0, 0, 0, 11, 0, 0, 0, 0,
        0, 0, 0, 1, 0, 0, 0, 164, 129, 32, 0, 0, 0, 98, 47, 104, 101, 108, 108, 111,
        46, 116, 120, 116, 80, 75, 5, 6, 0, 0, 0, 0, 2, 0, 2, 0, 105, 0, 0, 0,
        79, 0, 0, 0, 0, 0,
    ];

    /// The same, streamed through a pipe (`zip -r -X -fz- - b | cat`): the
    /// file deflated, with a data descriptor after it.
    #[rustfmt::skip]
    const STREAMED: &[u8] = &[
        80, 75, 3, 4, 10, 0, 0, 0, 0, 0, 0, 96, 95, 93, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 98, 47, 80, 75, 3, 4, 20, 0, 8, 0,
        8, 0, 0, 96, 95, 93, 0, 0, 0, 0, 0, 0, 0, 0, 6, 0, 0, 0, 11, 0,
        0, 0, 98, 47, 104, 101, 108, 108, 111, 46, 116, 120, 116, 203, 72, 205, 201, 201, 231, 2,
        0, 80, 75, 7, 8, 32, 48, 58, 54, 8, 0, 0, 0, 6, 0, 0, 0, 80, 75, 1,
        2, 30, 3, 10, 0, 0, 0, 0, 0, 0, 96, 95, 93, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 16, 0, 237, 65, 0,
        0, 0, 0, 98, 47, 80, 75, 1, 2, 30, 3, 20, 0, 8, 0, 8, 0, 0, 96, 95,
        93, 32, 48, 58, 54, 8, 0, 0, 0, 6, 0, 0, 0, 11, 0, 0, 0, 0, 0, 0,
        0, 1, 0, 0, 0, 164, 129, 32, 0, 0, 0, 98, 47, 104, 101, 108, 108, 111, 46, 116,
        120, 116, 80, 75, 5, 6, 0, 0, 0, 0, 2, 0, 2, 0, 105, 0, 0, 0, 97, 0,
        0, 0, 0, 0,
    ];

    /// An entry written by hand: name, made by, mode or attributes,
    /// flags, method, data as stored, and the size it says it unpacks to.
    type Handmade<'a> = (&'a str, u16, u32, u16, u16, &'a [u8], u32);

    /// Writes an archive by hand, entry by entry.
    fn handmade(entries: &[Handmade<'_>]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, made_by, attributes, flags, method, data, size) in entries {
            let offset = out.len() as u32;
            let mut crc = Crc::new();
            crc.update(data);
            let common = |v: &mut Vec<u8>| {
                v.extend_from_slice(&flags.to_le_bytes());
                v.extend_from_slice(&method.to_le_bytes());
                v.extend_from_slice(&[0, 0, 0x21, 0]);
                v.extend_from_slice(&crc.value().to_le_bytes());
                v.extend_from_slice(&(data.len() as u32).to_le_bytes());
                v.extend_from_slice(&size.to_le_bytes());
                v.extend_from_slice(&(name.len() as u16).to_le_bytes());
                v.extend_from_slice(&0u16.to_le_bytes());
            };
            out.extend_from_slice(&LOCAL.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            common(&mut out);
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);
            central.extend_from_slice(&CENTRAL.to_le_bytes());
            central.extend_from_slice(&made_by.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            common(&mut central);
            central.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
            central.extend_from_slice(&attributes.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let (cd_offset, cd_size) = (out.len() as u32, central.len() as u32);
        out.extend_from_slice(&central);
        out.extend_from_slice(&END.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        let n = entries.len() as u16;
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out
    }

    #[test]
    fn what_attacks_use_is_refused() {
        let file = |mode: u32| mode << 16;
        let unix = MADE_BY;
        let cases: &[(&[Handmade<'_>], &str)] = &[
            (&[("../evil", unix, file(0o100644), 0, 0, b"x", 1)], "not a path in the bundle"),
            (&[("/etc/passwd", unix, file(0o100644), 0, 0, b"x", 1)], "not a path in the bundle"),
            (&[("a\\..\\b", unix, file(0o100644), 0, 0, b"x", 1)], "not a path in the bundle"),
            (&[("link", unix, file(0o120777), 0, 0, b"/etc", 4)], "symbolic link"),
            (&[("dev", unix, file(0o020644), 0, 0, b"", 0)], "device file"),
            (&[("suid", unix, file(0o104755), 0, 0, b"x", 1)], "set-ID"),
            (&[("sgid", unix, file(0o102755), 0, 0, b"x", 1)], "set-ID"),
            (&[("secret", unix, file(0o100644), 1, 0, b"x", 1)], "encrypted"),
            (&[("bz", unix, file(0o100644), 0, 12, b"x", 1)], "stored or deflated"),
            (
                &[
                    ("a", unix, file(0o100644), 0, 0, b"x", 1),
                    ("a", unix, file(0o100644), 0, 0, b"y", 1),
                ],
                "twice",
            ),
            (
                &[
                    ("a", unix, file(0o100644), 0, 0, b"x", 1),
                    ("a/b", unix, file(0o100644), 0, 0, b"y", 1),
                ],
                "a folder of others",
            ),
            (&[("big", unix, file(0o100644), 0, 0, b"x", u32::MAX)], "zip64"),
        ];
        for (entries, why) in cases {
            let bytes = handmade(entries);
            let error = Archive::read(&bytes[..]).err().unwrap_or_default();
            assert!(error.contains(why), "{entries:?}: {error}");
        }
        // A stored file that says it is bigger than its bytes, and a
        // deflated one whose bytes unpack to less than it says.
        let bytes = handmade(&[("short", unix, file(0o100644), 0, 0, b"abc", 10)]);
        assert!(Archive::read(&bytes[..]).err().unwrap_or_default().contains("sizes differ"));
        let deflated = miniz_oxide::deflate::compress_to_vec(b"abc", 6);
        let bytes = handmade(&[("short", unix, file(0o100644), 0, DEFLATED, &deflated, 10)]);
        let archive = Archive::read(&bytes[..]).unwrap();
        let error = archive.read_small(&archive.entries[0], 100).unwrap_err();
        assert!(error.contains("fewer bytes") || error.contains("CRC"), "{error}");
    }

    #[test]
    fn a_zip_bomb_is_refused_before_it_unpacks() {
        let zeros = vec![0u8; 40 << 20];
        let bytes = pack(&[("zeros", false, &zeros)]);
        assert!(bytes.len() < 100_000);
        let error = Archive::read(&bytes[..]).err().unwrap();
        assert!(error.contains("fill the disk"), "{error}");
    }

    #[test]
    fn damage_is_found() {
        let text = b"some text, some text, some text\n".repeat(50);
        let bytes = pack(&[("a", false, &text), ("b", false, b"plain")]);
        // Each byte of the archive changed in turn: every change is found,
        // by the directory's checks or by unpacking, and none panics.
        for i in 0..bytes.len() {
            let mut damaged = bytes.clone();
            damaged[i] ^= 0x41;
            let Ok(archive) = Archive::read(&damaged[..]) else { continue };
            let read: Result<Vec<Vec<u8>>, String> =
                archive.entries.iter().map(|e| archive.read_small(e, 1 << 20)).collect();
            if let Ok(files) = read {
                // Only what no check covers may change: the version and
                // the time, which nothing reads.
                assert_eq!(files, [text.clone(), b"plain".to_vec()], "byte {i}");
            }
        }
        for cut in [0, 10, 21, bytes.len() / 2, bytes.len() - 1] {
            assert!(Archive::read(&bytes[..cut]).is_err(), "cut at {cut}");
        }
    }
}
