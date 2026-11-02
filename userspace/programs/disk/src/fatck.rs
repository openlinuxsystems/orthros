// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Checking a FAT filesystem, and mending what can be mended safely, for
//! fsck-fat(8).
//!
//! [`check`] reads the boot sector (or FAT32's backup of it), the
//! allocation tables and every directory, and follows every file's chain
//! of clusters. It finds what dosfstools' fsck.fat finds of the same
//! kinds: tables that differ, chains that are broken, loop or are shared
//! by two files, files whose size their chain does not hold, clusters
//! marked used that no file has, long names whose short entry is gone,
//! `.` and `..` that point elsewhere, a volume not cleanly unmounted, and
//! FSInfo's count of free clusters. With `repair` it mends each by
//! taking away what is broken and never by guessing: a file is cut where
//! its chain breaks or meets another's, lost clusters are freed, the
//! first table is copied over the others, and the counts and flags are
//! set right. Nothing is written without `repair`.

use std::collections::VecDeque;

use crate::device::Device;
use crate::fat::Bpb;

/// What to do.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// Mend what is found.
    pub repair: bool,
}

/// What was found.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// What is wrong, each with whether it was mended.
    pub problems: Vec<(String, bool)>,
    /// How many files there are.
    pub files: u32,
    /// How many directories, the root among them.
    pub directories: u32,
    /// Clusters files and directories hold.
    pub used: u32,
    /// Clusters free, after mending.
    pub free: u32,
    /// How many data clusters there are.
    pub clusters: u32,
    /// The size of a cluster.
    pub cluster_bytes: u32,
    /// 12, 16 or 32.
    pub bits: u8,
    /// The volume's label, from its root directory or its boot sector.
    pub label: Option<String>,
}

impl Report {
    /// Whether nothing is wrong, or all that was is mended.
    pub fn clean(&self) -> bool {
        self.problems.iter().all(|(_, mended)| *mended)
    }
}

/// The deepest directories followed: deeper ones are reported, not read.
const MAX_DEPTH: usize = 256;

/// A FAT in memory.
struct Table {
    bytes: Vec<u8>,
    bits: u8,
}

impl Table {
    fn get(&self, n: u32) -> u32 {
        let n = n as usize;
        match self.bits {
            12 => {
                let at = n + n / 2;
                let pair = u16::from_le_bytes([self.bytes[at], self.bytes[at + 1]]);
                u32::from(if n.is_multiple_of(2) { pair & 0xfff } else { pair >> 4 })
            }
            16 => u32::from(u16::from_le_bytes([self.bytes[2 * n], self.bytes[2 * n + 1]])),
            _ => {
                let at = 4 * n;
                u32::from_le_bytes(self.bytes[at..at + 4].try_into().unwrap_or_default()) & 0x0fff_ffff
            }
        }
    }

    fn set(&mut self, n: u32, value: u32) {
        let n = n as usize;
        match self.bits {
            12 => {
                let at = n + n / 2;
                let mut pair = u16::from_le_bytes([self.bytes[at], self.bytes[at + 1]]);
                let value = value as u16 & 0xfff;
                pair = if n.is_multiple_of(2) { (pair & 0xf000) | value } else { (pair & 0x000f) | (value << 4) };
                self.bytes[at..at + 2].copy_from_slice(&pair.to_le_bytes());
            }
            16 => self.bytes[2 * n..2 * n + 2].copy_from_slice(&(value as u16).to_le_bytes()),
            _ => {
                let at = 4 * n;
                let old = u32::from_le_bytes(self.bytes[at..at + 4].try_into().unwrap_or_default());
                let new = (old & 0xf000_0000) | (value & 0x0fff_ffff);
                self.bytes[at..at + 4].copy_from_slice(&new.to_le_bytes());
            }
        }
    }

    /// The value at and above which an entry ends a chain.
    fn end(&self) -> u32 {
        match self.bits {
            12 => 0xff8,
            16 => 0xfff8,
            _ => 0x0fff_fff8,
        }
    }

    /// The value of a bad cluster.
    fn bad(&self) -> u32 {
        self.end() - 1
    }
}

/// A directory entry that will be written again.
struct Patch {
    offset: u64,
    entry: [u8; 32],
}

/// A directory to read: where its clusters are (none for FAT12 and
/// FAT16's root), its cluster, its parent's, its path and depth.
struct Dir {
    first: u32,
    /// Its clusters, owned already; none for FAT12 and FAT16's root.
    chain: Vec<u32>,
    parent: u32,
    path: String,
    depth: usize,
}

struct Checker<'a> {
    device: &'a Device,
    bpb: Bpb,
    fat: Table,
    /// Which file or directory holds each cluster: 0 for none, else its
    /// number from 1.
    owner: Vec<u32>,
    next_owner: u32,
    patches: Vec<Patch>,
    report: Report,
    repair: bool,
    /// Whether the tables were changed, and must be written.
    fat_changed: bool,
    /// FAT32's FSInfo sector, mended, and where it goes.
    pending_info: Option<(u64, Vec<u8>)>,
    /// Where the boot sector's dirty flag is, if it is to be cleared.
    clear_dirty: Option<usize>,
}

/// The short name in `entry` as people write it, `README.TXT`.
fn short_name(entry: &[u8]) -> String {
    let mut base: Vec<u8> = entry[..8].to_vec();
    if base[0] == 0x05 {
        base[0] = 0xe5;
    }
    let base = String::from_utf8_lossy(&base).trim_end().to_string();
    let ext = String::from_utf8_lossy(&entry[8..11]).trim_end().to_string();
    if ext.is_empty() { base } else { format!("{base}.{ext}") }
}

/// The checksum of a short name that its long name's entries carry.
fn name_checksum(name: &[u8]) -> u8 {
    name[..11].iter().fold(0u8, |sum, &b| sum.rotate_right(1).wrapping_add(b))
}

/// Checks the FAT filesystem on `device`, and with `options.repair`
/// mends it.
///
/// # Errors
///
/// In words, if it is no FAT filesystem that can be checked: no boot
/// sector reads, or the tables cannot be read.
pub fn check(device: &Device, options: Options) -> Result<Report, String> {
    let read = |offset: u64, len: usize| {
        let mut out = vec![0u8; len];
        device.read_at(offset, &mut out).map(|()| out).map_err(|e| format!("cannot read: {e}"))
    };
    let mut first = read(0, 512)?;
    let mut problems = Vec::new();
    let bpb = match Bpb::parse(&first) {
        Ok(bpb) => bpb,
        Err(why) => {
            // FAT32 keeps a copy at sector 6, of 512 or 4096 bytes.
            let backup = [512u64, 4096].into_iter().find_map(|size| {
                let sector = read(6 * size, 512).ok()?;
                Bpb::parse(&sector).ok().filter(|bpb| bpb.bits == 32 && u64::from(bpb.sector_size) == size)
            });
            let Some(bpb) = backup else {
                return Err(format!("this is no FAT filesystem that can be checked: {why}"));
            };
            let size = u64::from(bpb.sector_size);
            let mended = options.repair
                && read(6 * size, size as usize).is_ok_and(|copy| device.write_at(0, &copy).is_ok());
            problems.push((
                format!(
                    "The boot sector is damaged ({why}); its backup at sector 6 is good, and was {}.",
                    if mended { "copied over it" } else { "not copied over it" }
                ),
                mended,
            ));
            bpb
        }
    };
    if problems.iter().any(|(_, mended)| *mended) {
        first = read(0, 512)?;
    }
    let total_bytes = bpb.total_sectors * u64::from(bpb.sector_size);
    if total_bytes > device.len() {
        return Err(format!(
            "the filesystem is {} bytes but its device only {}; was the partition made smaller?",
            total_bytes,
            device.len()
        ));
    }
    let fat_bytes = bpb.fat_sectors as usize * bpb.sector_size as usize;
    let fat_at = u64::from(bpb.reserved) * u64::from(bpb.sector_size);
    let active = if bpb.bits == 32 && bpb.ext_flags & 0x80 != 0 { u32::from(bpb.ext_flags & 0xf) } else { 0 };
    if active >= bpb.fats {
        return Err(format!("the active table is number {active}, of {}", bpb.fats));
    }
    let fat = Table { bytes: read(fat_at + u64::from(active) * fat_bytes as u64, fat_bytes)?, bits: bpb.bits };
    let clusters = bpb.clusters;
    let mut checker = Checker {
        device,
        owner: vec![0; clusters as usize + 2],
        next_owner: 0,
        patches: Vec::new(),
        report: Report {
            problems,
            clusters,
            cluster_bytes: bpb.cluster_bytes(),
            bits: bpb.bits,
            ..Report::default()
        },
        repair: options.repair,
        fat_changed: false,
        pending_info: None,
        clear_dirty: None,
        fat,
        bpb,
    };
    checker.check_tables(fat_at, fat_bytes, active)?;
    checker.check_flags(&first);
    checker.walk()?;
    checker.lost();
    checker.fsinfo(&first)?;
    checker.backup_boot(&first)?;
    checker.write(fat_at, fat_bytes)?;
    let free = (2..clusters + 2).filter(|&n| checker.fat.get(n) == 0).count() as u32;
    checker.report.free = free;
    Ok(checker.report)
}

impl Checker<'_> {
    fn problem(&mut self, text: String, mendable: bool) -> bool {
        let mended = mendable && self.repair;
        self.report.problems.push((text, mended));
        mended
    }

    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>, String> {
        let mut out = vec![0u8; len];
        self.device.read_at(offset, &mut out).map(|()| out).map_err(|e| format!("cannot read: {e}"))
    }

    /// The other tables are the active one's copies.
    fn check_tables(&mut self, fat_at: u64, fat_bytes: usize, active: u32) -> Result<(), String> {
        let mirrored = !(self.bpb.bits == 32 && self.bpb.ext_flags & 0x80 != 0);
        // Only the entries of clusters that are; the rest may differ.
        let used = ((u64::from(self.bpb.clusters) + 2) * u64::from(self.bpb.bits)).div_ceil(8) as usize;
        if mirrored {
            for n in (0..self.bpb.fats).filter(|&n| n != active) {
                let other = self.read(fat_at + u64::from(n) * fat_bytes as u64, fat_bytes)?;
                if other[..used] != self.fat.bytes[..used] {
                    let text = format!("Allocation table {} differs from table {}.", n + 1, active + 1);
                    if self.problem(text, true) {
                        self.fat_changed = true;
                    }
                }
            }
        }
        let media = self.fat.get(0) & 0xff;
        if media != u32::from(self.bpb.media) {
            let text = format!(
                "The first table entry's media byte, {media:#04x}, is not the boot sector's, {:#04x}.",
                self.bpb.media
            );
            if self.problem(text, true) {
                let head = self.fat.get(0) & !0xff | u32::from(self.bpb.media);
                self.fat.set(0, head);
                self.fat_changed = true;
            }
        }
        Ok(())
    }

    /// Whether the volume was cleanly unmounted: FAT16 and FAT32 say so in
    /// cluster 1's entry, and Linux in the boot sector's reserved byte.
    fn check_flags(&mut self, boot: &[u8]) {
        let (clean, no_error) = match self.bpb.bits {
            16 => (0x8000, 0x4000),
            32 => (0x0800_0000, 0x0400_0000),
            _ => (0, 0),
        };
        let entry = self.fat.get(1);
        if clean != 0 && entry & clean == 0 {
            let text = "The volume was not cleanly unmounted.".to_string();
            if self.problem(text, true) {
                self.fat.set(1, entry | clean);
                self.fat_changed = true;
            }
        }
        if no_error != 0 && self.fat.get(1) & no_error == 0 {
            let text = "The volume says a disk error was met while it was mounted.".to_string();
            if self.problem(text, true) {
                self.fat.set(1, self.fat.get(1) | no_error);
                self.fat_changed = true;
            }
        }
        let dirty_at = if self.bpb.bits == 32 { 65 } else { 37 };
        if boot[dirty_at] & 1 != 0 {
            let text = "The boot sector's dirty flag is set: the volume was not cleanly unmounted."
                .to_string();
            if self.problem(text, true) {
                self.clear_dirty = Some(dirty_at);
            }
        }
    }

    /// The clusters of the chain from `first`, as far as it goes right;
    /// why it stops early, if it does, and the cluster it shares with
    /// another's chain, if it meets one.
    fn chain(&self, first: u32, me: u32) -> (Vec<u32>, Option<String>) {
        let mut found = Vec::new();
        let max = self.bpb.clusters + 1;
        let mut at = first;
        loop {
            if at < 2 || at > max {
                return (found, Some(format!("points to cluster {at}, which is not one")));
            }
            let owner = self.owner[at as usize];
            if owner == me {
                return (found, Some(format!("loops back to cluster {at}")));
            }
            if owner != 0 {
                return (found, Some(format!("meets another file's chain at cluster {at}")));
            }
            let next = self.fat.get(at);
            if next == 0 {
                return (found, Some(format!("reaches cluster {at}, which is marked free")));
            }
            if next == self.fat.bad() {
                return (found, Some(format!("reaches cluster {at}, which is marked bad")));
            }
            found.push(at);
            if next >= self.fat.end() {
                return (found, None);
            }
            at = next;
        }
    }

    /// Takes `chain` for `me`.
    fn own(&mut self, chain: &[u32], me: u32) {
        for &c in chain {
            self.owner[c as usize] = me;
        }
        self.report.used += chain.len() as u32;
    }

    /// Ends the chain after `chain`'s clusters: the last is made its end.
    fn cut(&mut self, chain: &[u32]) {
        if let Some(&last) = chain.last() {
            self.fat.set(last, 0x0fff_ffff);
            self.fat_changed = true;
        }
    }

    /// Reads every directory from the root.
    fn walk(&mut self) -> Result<(), String> {
        let mut queue = VecDeque::new();
        queue.push_back(Dir { first: 0, chain: Vec::new(), parent: 0, path: String::new(), depth: 0 });
        self.next_owner = 1;
        let root_owner = self.next_owner;
        if self.bpb.bits == 32 {
            let root = self.bpb.root_cluster;
            let (chain, broken) = self.chain(root, root_owner);
            if chain.is_empty() {
                return Err(format!("the root directory's first cluster, {root}, is not one"));
            }
            if let Some(why) = broken {
                let text = format!("The root directory's chain {why}; it was cut there.");
                if self.problem(text, true) {
                    self.cut(&chain);
                }
            }
            self.own(&chain, root_owner);
            queue[0].first = root;
            queue[0].chain = chain;
        }
        self.report.directories += 1;
        while let Some(dir) = queue.pop_front() {
            self.directory(&dir, &mut queue)?;
        }
        Ok(())
    }

    /// Where each entry of a directory is: the byte offset of each of its
    /// clusters' starts, or of FAT12 and FAT16's root.
    fn places(&self, dir: &Dir) -> Vec<(u64, usize)> {
        if dir.first == 0 {
            let at = (u64::from(self.bpb.reserved) + u64::from(self.bpb.fats) * u64::from(self.bpb.fat_sectors))
                * u64::from(self.bpb.sector_size);
            return vec![(at, self.bpb.root_entries as usize * 32)];
        }
        dir.chain
            .iter()
            .map(|&c| (self.bpb.cluster_offset(c), self.bpb.cluster_bytes() as usize))
            .collect()
    }

    fn directory(&mut self, dir: &Dir, queue: &mut VecDeque<Dir>) -> Result<(), String> {
        let places = self.places(dir);
        let mut entries: Vec<(u64, [u8; 32])> = Vec::new();
        for &(at, len) in &places {
            let bytes = self.read(at, len)?;
            for (i, &e) in bytes.as_chunks::<32>().0.iter().enumerate() {
                entries.push((at + 32 * i as u64, e));
            }
        }
        let is_root = dir.depth == 0;
        let mut long: Vec<(u64, [u8; 32])> = Vec::new();
        let mut long_name: Vec<u16> = Vec::new();
        let mut labels = 0;
        let mut index = 0;
        for (offset, entry) in entries {
            index += 1;
            if entry[0] == 0 {
                break;
            }
            if entry[0] == 0xe5 {
                self.orphans(&long, dir);
                long.clear();
                long_name.clear();
                continue;
            }
            let attr = entry[11];
            if attr & 0x3f == 0x0f {
                let ord = entry[0];
                if ord & 0x40 != 0 {
                    self.orphans(&long, dir);
                    long.clear();
                    long_name.clear();
                }
                long.push((offset, entry));
                let mut units: Vec<u16> = Vec::new();
                for range in [1..11, 14..26, 28..32] {
                    units.extend(entry[range].as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)));
                }
                let mut part = units;
                part.append(&mut long_name);
                long_name = part;
                continue;
            }
            // A short entry, with the long name before it, if it is its.
            let name = short_name(&entry);
            let mut shown = name.clone();
            if !long.is_empty() {
                let sum = name_checksum(&entry);
                let count = (long[0].1[0] & 0x1f) as usize;
                let ordered = long.len() == count
                    && long.iter().enumerate().all(|(i, (_, e))| {
                        (e[0] & 0x1f) as usize == count - i && e[13] == sum
                    });
                if ordered {
                    let end = long_name.iter().position(|&u| u == 0).unwrap_or(long_name.len());
                    shown = String::from_utf16_lossy(&long_name[..end]);
                } else {
                    self.orphans(&long, dir);
                }
                long.clear();
                long_name.clear();
            }
            let path = if dir.path.is_empty() { format!("/{shown}") } else { format!("{}/{shown}", dir.path) };
            if attr & 0x08 != 0 && attr & 0x10 == 0 {
                labels += 1;
                if !is_root || labels > 1 {
                    let text = format!("{path}: a volume label {}; it was taken out.",
                        if is_root { "after the first" } else { "outside the root directory" });
                    if self.problem(text, true) {
                        let mut gone = entry;
                        gone[0] = 0xe5;
                        self.patches.push(Patch { offset, entry: gone });
                    }
                } else {
                    self.report.label = Some(String::from_utf8_lossy(&entry[..11]).trim_end().to_string());
                }
                continue;
            }
            if let Some(c) = entry[..11].iter().enumerate().find_map(|(i, &b)| {
                let bad = b < 0x20 && !(i == 0 && b == 0x05) || b"\"*+,/:;<=>?[\\]|".contains(&b);
                bad.then_some(b)
            }) {
                self.problem(format!("{path}: its short name holds the byte {c:#04x}, which no short name may."), false);
            }
            let hi = if self.bpb.bits == 32 { u32::from(u16::from_le_bytes([entry[20], entry[21]])) } else { 0 };
            let start = hi << 16 | u32::from(u16::from_le_bytes([entry[26], entry[27]]));
            let size = u32::from_le_bytes([entry[28], entry[29], entry[30], entry[31]]);
            let dot = &entry[..11] == b".          ";
            let dotdot = &entry[..11] == b"..         ";
            if dot || dotdot {
                if is_root {
                    self.problem(format!("The root directory has a \"{}\" entry.", if dot { "." } else { ".." }), false);
                    continue;
                }
                let want = if dot { dir.first } else { dir.parent };
                let first_two = index <= 2;
                if start != want || !first_two {
                    let text = format!(
                        "{}: its \"{}\" entry points to cluster {start}, not {want}.",
                        dir.path,
                        if dot { "." } else { ".." }
                    );
                    if self.problem(text, first_two) {
                        let mut fixed = entry;
                        fixed[20..22].copy_from_slice(&((want >> 16) as u16).to_le_bytes());
                        fixed[26..28].copy_from_slice(&(want as u16).to_le_bytes());
                        self.patches.push(Patch { offset, entry: fixed });
                    }
                }
                continue;
            }
            self.next_owner += 1;
            let me = self.next_owner;
            if attr & 0x10 != 0 {
                self.report.directories += 1;
                if size != 0 {
                    let text = format!("{path}: a directory with a size, {size}; it was set to 0.");
                    if self.problem(text, true) {
                        let mut fixed = entry;
                        fixed[28..32].fill(0);
                        self.patches.push(Patch { offset, entry: fixed });
                    }
                }
                let (chain, broken) = self.chain(start, me);
                if chain.is_empty() {
                    let why = broken.unwrap_or_default();
                    let text = format!("{path}: a directory whose first cluster {why}; it was taken out.");
                    if self.problem(text, true) {
                        let mut gone = entry;
                        gone[0] = 0xe5;
                        self.patches.push(Patch { offset, entry: gone });
                    }
                    continue;
                }
                if let Some(why) = broken {
                    let text = format!("{path}: the directory's chain {why}; it was cut there.");
                    if self.problem(text, true) {
                        self.cut(&chain);
                    }
                }
                self.own(&chain, me);
                if dir.depth + 1 >= MAX_DEPTH {
                    self.problem(format!("{path}: directories nest deeper than {MAX_DEPTH}; those below were not read."), false);
                    continue;
                }
                queue.push_back(Dir {
                    first: start,
                    chain,
                    parent: if is_root { 0 } else { dir.first },
                    path,
                    depth: dir.depth + 1,
                });
                continue;
            }
            self.report.files += 1;
            let cluster = self.bpb.cluster_bytes();
            if size == 0 {
                if start != 0 {
                    let text = format!("{path}: an empty file that holds clusters; they were freed.");
                    if self.problem(text, true) {
                        let mut fixed = entry;
                        fixed[20..22].fill(0);
                        fixed[26..28].fill(0);
                        self.patches.push(Patch { offset, entry: fixed });
                    }
                }
                continue;
            }
            let (chain, broken) = self.chain(start, me);
            let wanted = size.div_ceil(cluster) as usize;
            let mut keep = chain.len().min(wanted);
            let mut why = broken.clone();
            if broken.is_none() && chain.len() > wanted {
                why = Some(format!("holds {} clusters, more than its size needs", chain.len()));
            } else if broken.is_none() && chain.len() < wanted {
                why = Some(format!("ends after {} clusters, fewer than its size needs", chain.len()));
            }
            if let Some(why) = why {
                let short = keep < wanted;
                let new_size = if short { (keep as u64 * u64::from(cluster)) as u32 } else { size };
                let text = if short {
                    format!("{path}: its chain {why}; the file was cut to {new_size} bytes.")
                } else {
                    format!("{path}: its chain {why}; the rest was freed.")
                };
                if self.problem(text, true) {
                    keep = keep.min(chain.len());
                    if keep == 0 {
                        let mut fixed = entry;
                        fixed[20..22].fill(0);
                        fixed[26..28].fill(0);
                        fixed[28..32].fill(0);
                        self.patches.push(Patch { offset, entry: fixed });
                    } else {
                        self.cut(&chain[..keep]);
                        if short {
                            let mut fixed = entry;
                            fixed[28..32].copy_from_slice(&new_size.to_le_bytes());
                            self.patches.push(Patch { offset, entry: fixed });
                        }
                    }
                } else {
                    keep = chain.len();
                }
            }
            let held = chain[..keep.min(chain.len())].to_vec();
            self.own(&held, me);
        }
        self.orphans(&long, dir);
        Ok(())
    }

    /// Long name entries with no short entry of theirs after them.
    fn orphans(&mut self, long: &[(u64, [u8; 32])], dir: &Dir) {
        if long.is_empty() {
            return;
        }
        let place = if dir.path.is_empty() { "/" } else { &dir.path };
        let text = format!("{place}: {} long name entries belong to no file; they were taken out.", long.len());
        if self.problem(text, true) {
            for (offset, entry) in long {
                let mut gone = *entry;
                gone[0] = 0xe5;
                self.patches.push(Patch { offset: *offset, entry: gone });
            }
        }
    }

    /// Clusters marked used that nothing holds.
    fn lost(&mut self) {
        let max = self.bpb.clusters + 1;
        let lost: Vec<u32> = (2..=max)
            .filter(|&n| {
                let v = self.fat.get(n);
                v != 0 && v != self.fat.bad() && self.owner[n as usize] == 0
            })
            .collect();
        if lost.is_empty() {
            return;
        }
        // A chain starts where no lost cluster points.
        let mut pointed = vec![false; max as usize + 1];
        for &n in &lost {
            let next = self.fat.get(n);
            if (2..=max).contains(&next) {
                pointed[next as usize] = true;
            }
        }
        let chains = lost.iter().filter(|&&n| !pointed[n as usize]).count().max(1);
        let text = format!(
            "{} clusters in {chains} {} are marked used but no file holds them; they were freed.",
            lost.len(),
            if chains == 1 { "chain" } else { "chains" }
        );
        if self.problem(text, true) {
            for n in lost {
                self.fat.set(n, 0);
            }
            self.fat_changed = true;
        }
    }

    /// FAT32's FSInfo sector: its signatures and its count of free
    /// clusters.
    fn fsinfo(&mut self, boot: &[u8]) -> Result<(), String> {
        let _ = boot;
        if self.bpb.bits != 32 || self.bpb.fsinfo == 0 || self.bpb.fsinfo >= self.bpb.reserved {
            return Ok(());
        }
        let at = u64::from(self.bpb.fsinfo) * u64::from(self.bpb.sector_size);
        let mut info = self.read(at, 512)?;
        let u32_at = |b: &[u8], at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        let signed = u32_at(&info, 0) == 0x4161_5252 && u32_at(&info, 484) == 0x6141_7272;
        let max = self.bpb.clusters + 1;
        let free = (2..=max).filter(|&n| {
            let v = self.fat.get(n);
            // Clusters the repair frees count as free.
            v == 0
        }).count() as u32;
        let said = u32_at(&info, 488);
        let next = u32_at(&info, 492);
        let mut fix = false;
        if !signed {
            fix |= self.problem("The FSInfo sector's signatures are wrong.".to_string(), true);
        } else if said != 0xffff_ffff && said != free {
            let text = format!("The FSInfo sector counts {said} free clusters; there are {free}.");
            fix |= self.problem(text, true);
        }
        if signed && next != 0xffff_ffff && (next < 2 || next > max) {
            let text = format!("The FSInfo sector's next free cluster, {next}, is not one.");
            fix |= self.problem(text, true);
        }
        if fix {
            info[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
            info[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
            info[488..492].copy_from_slice(&free.to_le_bytes());
            let first_free = (2..=max).find(|&n| self.fat.get(n) == 0).unwrap_or(0xffff_ffff);
            info[492..496].copy_from_slice(&first_free.to_le_bytes());
            info[508..512].copy_from_slice(&0xaa55_0000u32.to_le_bytes());
            self.pending_info = Some((at, info));
        }
        Ok(())
    }

    /// FAT32's backup boot sector is the boot sector's copy.
    fn backup_boot(&mut self, boot: &[u8]) -> Result<(), String> {
        let backup = self.bpb.backup_boot;
        if self.bpb.bits != 32 || backup == 0 || backup >= self.bpb.reserved {
            return Ok(());
        }
        let at = u64::from(backup) * u64::from(self.bpb.sector_size);
        let copy = self.read(at, 512)?;
        // The dirty flag may differ: Linux sets it in the first only.
        let mut a = boot[..512].to_vec();
        let mut b = copy;
        a[65] &= !1;
        b[65] &= !1;
        if a != b {
            let text = "The backup boot sector differs from the boot sector.".to_string();
            if self.problem(text, true) {
                self.device.write_at(at, &a).map_err(|e| format!("cannot write: {e}"))?;
            }
        }
        Ok(())
    }

    /// Writes what was mended.
    fn write(&mut self, fat_at: u64, fat_bytes: usize) -> Result<(), String> {
        if !self.repair {
            return Ok(());
        }
        let w = |device: &Device, at: u64, bytes: &[u8]| device.write_at(at, bytes).map_err(|e| format!("cannot write: {e}"));
        if self.fat_changed {
            for n in 0..self.bpb.fats {
                w(self.device, fat_at + u64::from(n) * fat_bytes as u64, &self.fat.bytes)?;
            }
        }
        for patch in &self.patches {
            w(self.device, patch.offset, &patch.entry)?;
        }
        if let Some((at, info)) = self.pending_info.take() {
            w(self.device, at, &info)?;
        }
        if let Some(at) = self.clear_dirty {
            let mut boot = self.read(0, 512)?;
            boot[at] &= !1;
            w(self.device, 0, &boot)?;
        }
        self.device.sync().map_err(|e| format!("cannot write: {e}"))
    }
}
