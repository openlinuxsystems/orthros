// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Room on a volume: its clusters and its file records, which are free
//! and taking them.
//!
//! `$Bitmap`'s data has a bit for each cluster, set when it is in use,
//! and the master file table's `$BITMAP` one for each record. Both are
//! read and written through the block cache, as the rest of the metadata
//! is, a sector's worth at a time.
//!
//! A file's clusters are taken after its last one while those are free,
//! so that it stays in one run, and otherwise from where the last ones
//! taken ended. The clusters after the master file table (an eighth of
//! the volume, as Windows keeps them) are its zone, which it grows into
//! and other files take only once the rest is full, as on Windows: so
//! the table stays in a few runs that its first record has room for.

use radix_block::cache::BlockCache;
use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result};

use crate::boot::Geometry;
use crate::runs::{HOLE, Runs};

/// The bytes of a bitmap read at a time: a sector's worth.
const CHUNK: usize = 512;

/// Reads `buf.len()` bytes from byte `at` of the attribute whose clusters
/// `runs` are, through `cache`; holes read as zeros.
pub(crate) fn read_runs(
    cache: &BlockCache,
    geo: &Geometry,
    runs: &Runs,
    at: u64,
    buf: &mut [u8],
) -> Result<()> {
    let cs = u64::from(geo.cluster_size);
    let mut done = 0;
    while done < buf.len() {
        let pos = at + done as u64;
        let (lcn, n) = runs.map(pos / cs).ok_or(Error::Corrupt)?;
        let within = pos % cs;
        let len = (n * cs - within).min((buf.len() - done) as u64) as usize;
        let piece = &mut buf[done..done + len];
        if lcn == HOLE {
            piece.fill(0);
        } else {
            cache.read(geo.cluster_offset(lcn) + within, piece)?;
        }
        done += len;
    }
    Ok(())
}

/// Writes `data` at byte `at` of the attribute whose clusters `runs` are,
/// through `cache`: at the next sync, or now and durably if `through`.
pub(crate) fn write_runs(
    cache: &BlockCache,
    geo: &Geometry,
    runs: &Runs,
    at: u64,
    data: &[u8],
    through: bool,
) -> Result<()> {
    let cs = u64::from(geo.cluster_size);
    let mut done = 0;
    while done < data.len() {
        let pos = at + done as u64;
        let (lcn, n) = runs.map(pos / cs).ok_or(Error::Corrupt)?;
        if lcn == HOLE {
            return Err(Error::Corrupt);
        }
        let within = pos % cs;
        let len = (n * cs - within).min((data.len() - done) as u64) as usize;
        let offset = geo.cluster_offset(lcn) + within;
        let piece = &data[done..done + len];
        if through {
            cache.write_through(offset, piece)?;
        } else {
            cache.write(offset, piece)?;
        }
        done += len;
    }
    Ok(())
}

/// A bitmap in an attribute's clusters.
pub(crate) struct Bits<'a> {
    pub(crate) cache: &'a BlockCache,
    pub(crate) geo: &'a Geometry,
    pub(crate) runs: &'a Runs,
    /// How many bits count.
    pub(crate) len: u64,
}

impl Bits<'_> {
    /// The bytes holding bits `from..to`, read: the first byte's number,
    /// and how many.
    fn chunk(&self, from: u64, to: u64, buf: &mut [u8; CHUNK]) -> Result<(u64, usize)> {
        let byte = from / 8;
        let n = (CHUNK as u64).min((to - 1) / 8 + 1 - byte) as usize;
        read_runs(self.cache, self.geo, self.runs, byte, &mut buf[..n])?;
        Ok((byte, n))
    }

    /// The first clear bit in `from..to` and how many clear bits there are
    /// in a row from it, `max` at most.
    pub(crate) fn find_clear(&self, from: u64, to: u64, max: u64) -> Result<Option<(u64, u64)>> {
        let to = to.min(self.len);
        let mut buf = [0u8; CHUNK];
        let mut at = from;
        let mut start = None;
        let mut count = 0;
        while at < to {
            let (byte, n) = self.chunk(at, to, &mut buf)?;
            for (i, &b) in buf[..n].iter().enumerate() {
                if start.is_none() && b == 0xff {
                    continue;
                }
                let base = (byte + i as u64) * 8;
                for bit in 0..8 {
                    let k = base + bit;
                    if k < at {
                        continue;
                    }
                    if k >= to {
                        return Ok(start.map(|s| (s, count)));
                    }
                    let set = b & (1 << bit) != 0;
                    match start {
                        None if set => {}
                        None => {
                            start = Some(k);
                            count = 1;
                        }
                        Some(s) if set => return Ok(Some((s, count))),
                        Some(_) => count += 1,
                    }
                    if let Some(s) = start
                        && count == max
                    {
                        return Ok(Some((s, count)));
                    }
                }
            }
            at = (byte + n as u64) * 8;
        }
        Ok(start.map(|s| (s, count)))
    }

    /// How many clear bits there are in a row from bit `from`, `max` at
    /// most.
    pub(crate) fn clear_from(&self, from: u64, max: u64) -> Result<u64> {
        match self.find_clear(from, from.saturating_add(max), max)? {
            Some((start, count)) if start == from => Ok(count),
            _ => Ok(0),
        }
    }

    /// Sets bits `from..from + count` if `used`, or clears them.
    pub(crate) fn set(&self, from: u64, count: u64, used: bool) -> Result<()> {
        let end = from + count;
        if end > self.len {
            return Err(Error::Corrupt);
        }
        let mut buf = [0u8; CHUNK];
        let mut at = from;
        while at < end {
            let (byte, n) = self.chunk(at, end, &mut buf)?;
            for (i, b) in buf[..n].iter_mut().enumerate() {
                let base = (byte + i as u64) * 8;
                for bit in 0..8 {
                    let k = base + bit;
                    if (at..end).contains(&k) {
                        if used {
                            *b |= 1 << bit;
                        } else {
                            *b &= !(1 << bit);
                        }
                    }
                }
            }
            write_runs(self.cache, self.geo, self.runs, byte, &buf[..n], false)?;
            at = (byte + n as u64) * 8;
        }
        Ok(())
    }

    /// How many of its bits are set.
    pub(crate) fn count_set(&self) -> Result<u64> {
        let mut buf = [0u8; CHUNK];
        let mut used = 0;
        let mut at = 0;
        while at < self.len {
            let (byte, n) = self.chunk(at, self.len, &mut buf)?;
            for (i, &b) in buf[..n].iter().enumerate() {
                let first = (byte + i as u64) * 8;
                let bits = (self.len - first).min(8) as u32;
                let mask = if bits == 8 { 0xff } else { (1u8 << bits) - 1 };
                used += u64::from((b & mask).count_ones());
            }
            at = (byte + n as u64) * 8;
        }
        Ok(used)
    }
}

/// The clusters, as far as they are known.
pub(crate) struct Clusters {
    /// `$Bitmap`'s clusters.
    pub(crate) bitmap: Runs,
    /// How many are free; counted when first asked for.
    pub(crate) free: Option<u64>,
    /// Where to look for free ones first.
    pub(crate) hint: u64,
    /// The master file table's zone.
    pub(crate) zone: (u64, u64),
}

impl Clusters {
    /// The bitmap, on `cache`, of a volume of `geo`.
    pub(crate) fn bits<'a>(&'a self, cache: &'a BlockCache, geo: &'a Geometry) -> Bits<'a> {
        Bits { cache, geo, runs: &self.bitmap, len: geo.clusters }
    }

    /// How many clusters are free, counted the first time.
    pub(crate) fn free(&mut self, cache: &BlockCache, geo: &Geometry) -> Result<u64> {
        if let Some(free) = self.free {
            return Ok(free);
        }
        let used = self.bits(cache, geo).count_set()?;
        let free = geo.clusters - used.min(geo.clusters);
        self.free = Some(free);
        Ok(free)
    }

    /// Takes `want` clusters, in as few runs as it finds them: from
    /// `near` on while those are free, then for the master file table
    /// (`mft`) in one run if one is long enough, from its zone first, and
    /// for any other file from outside it while there are any. Returns the runs, `(first, count)`, in the
    /// order to use them. Fails with [`Error::NoSpace`] without taking
    /// any when too few are free.
    pub(crate) fn take(
        &mut self,
        cache: &BlockCache,
        geo: &Geometry,
        want: u64,
        near: Option<u64>,
        mft: bool,
    ) -> Result<KVec<(u64, u64)>> {
        let mut got = mem::vec();
        if want == 0 {
            return Ok(got);
        }
        if self.free(cache, geo)? < want {
            return Err(Error::NoSpace);
        }
        let total = geo.clusters;
        let (zs, ze) = self.zone;
        let hint = self.hint.clamp(ze, total);
        let regions: [(u64, u64); 4] = if mft {
            [(zs, ze), (ze, total), (0, zs), (0, 0)]
        } else {
            [(hint, total), (ze, hint), (0, zs), (zs, ze)]
        };
        let mut left = want;
        let result = (|| {
            let bits = self.bits(cache, geo);
            if let Some(near) = near.filter(|&n| n < total) {
                let n = bits.clear_from(near, left)?;
                if n > 0 {
                    bits.set(near, n, true)?;
                    mem::push(&mut got, (near, n))?;
                    left -= n;
                }
            }
            // The master file table's record 0 holds where all its
            // clusters are, so it grows by one run if any is long enough.
            if mft && left > 0 {
                for (from, to) in regions {
                    let mut at = from;
                    while let Some((start, n)) = bits.find_clear(at, to, left)? {
                        if n == left {
                            bits.set(start, n, true)?;
                            mem::push(&mut got, (start, n))?;
                            left = 0;
                            break;
                        }
                        at = start + n;
                    }
                    if left == 0 {
                        break;
                    }
                }
            }
            for (from, to) in regions {
                let mut at = from;
                while left > 0 && at < to {
                    let Some((start, n)) = bits.find_clear(at, to, left)? else { break };
                    bits.set(start, n, true)?;
                    mem::push(&mut got, (start, n))?;
                    left -= n;
                    at = start + n;
                }
            }
            if left > 0 { Err(Error::NoSpace) } else { Ok(()) }
        })();
        if let Err(err) = result {
            // What was taken goes back; the count was wrong, so it is
            // counted again next time.
            let bits = self.bits(cache, geo);
            for &(start, n) in &got {
                let _ = bits.set(start, n, false);
            }
            self.free = None;
            return Err(err);
        }
        if let Some(free) = &mut self.free {
            *free -= want;
        }
        if !mft && let Some(&(start, n)) = got.last() {
            self.hint = start + n;
        }
        Ok(got)
    }

    /// Gives back the `count` clusters from `first`.
    pub(crate) fn give(
        &mut self,
        cache: &BlockCache,
        geo: &Geometry,
        first: u64,
        count: u64,
    ) -> Result<()> {
        if count == 0 {
            return Ok(());
        }
        self.bits(cache, geo).set(first, count, false)?;
        if let Some(free) = &mut self.free {
            *free += count;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use radix_block::DeviceRef;
    use radix_block::ram::RamDisk;

    use super::*;

    /// A volume of 4096 clusters of 512 bytes whose bitmap is in clusters
    /// 1 and 2, all clear but those.
    fn volume() -> (BlockCache, Geometry, Runs) {
        let disk = mem::arc(RamDisk::with_blocks(4096, false, 512)).unwrap();
        let device: DeviceRef = disk;
        let cache = BlockCache::new(device);
        let geo = Geometry {
            sector_size: 512,
            cluster_size: 512,
            clusters: 4096,
            mft: 8,
            mft_mirror: 4000,
            record_size: 1024,
            index_size: 4096,
            serial: 0,
        };
        let mut runs = Runs::new();
        runs.append(1, 2).unwrap();
        (cache, geo, runs)
    }

    #[test]
    fn bits_go_on_and_off() {
        let (cache, geo, runs) = volume();
        let bits = Bits { cache: &cache, geo: &geo, runs: &runs, len: 4096 };
        assert_eq!(bits.find_clear(0, 4096, 10).unwrap(), Some((0, 10)));
        bits.set(3, 600, true).unwrap();
        assert_eq!(bits.count_set().unwrap(), 600);
        assert_eq!(bits.find_clear(0, 4096, 10).unwrap(), Some((0, 3)));
        assert_eq!(bits.find_clear(3, 4096, 10).unwrap(), Some((603, 10)));
        assert_eq!(bits.find_clear(3, 603, 10).unwrap(), None);
        assert_eq!(bits.clear_from(603, 5000).unwrap(), 4096 - 603);
        assert_eq!(bits.clear_from(602, 10).unwrap(), 0);
        bits.set(100, 7, false).unwrap();
        assert_eq!(bits.find_clear(3, 4096, 100).unwrap(), Some((100, 7)));
        assert_eq!(bits.count_set().unwrap(), 593);
    }

    #[test]
    fn clusters_come_from_outside_the_zone_first() {
        let (cache, geo, runs) = volume();
        let mut clusters = Clusters { bitmap: runs, free: None, hint: 0, zone: (8, 520) };
        // The bitmap's own clusters and the table's first are in use.
        clusters.bits(&cache, &geo).set(0, 16, true).unwrap();
        let got = clusters.take(&cache, &geo, 4, None, false).unwrap();
        assert_eq!(&got[..], &[(520, 4)]);
        let got = clusters.take(&cache, &geo, 3, Some(524), false).unwrap();
        assert_eq!(&got[..], &[(524, 3)]);
        let got = clusters.take(&cache, &geo, 2, None, true).unwrap();
        assert_eq!(&got[..], &[(16, 2)]);
        assert_eq!(clusters.free(&cache, &geo).unwrap(), 4096 - 16 - 9);
        clusters.give(&cache, &geo, 520, 4).unwrap();
        let got = clusters.take(&cache, &geo, 3, Some(530), false).unwrap();
        assert_eq!(&got[..], &[(530, 3)]);
        // Too many fails, taking none.
        let free = clusters.free(&cache, &geo).unwrap();
        assert_eq!(clusters.take(&cache, &geo, free + 1, None, false).unwrap_err(), Error::NoSpace);
        assert_eq!(clusters.free(&cache, &geo).unwrap(), free);
        // All of it, in pieces: outside the zone, then before, then in it.
        let got = clusters.take(&cache, &geo, free, None, false).unwrap();
        assert_eq!(got.iter().map(|r| r.1).sum::<u64>(), free);
        assert_eq!(clusters.bits(&cache, &geo).find_clear(0, 4096, 1).unwrap(), None);
    }
}
