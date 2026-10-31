// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Run lists: where a non-resident attribute's clusters are.
//!
//! An attribute's clusters are numbered from 0 (virtual cluster numbers,
//! VCNs) and lie in runs on the volume (at logical cluster numbers,
//! LCNs). A run list is packed: each run is a header byte, whose low
//! nibble is the length's size in bytes and whose high nibble the
//! offset's, then the length, then the offset from the last run's LCN,
//! both signed (Windows and ntfs-3g refuse a length with its top bit
//! set); a run with no offset is a hole, which reads as zeros. A zero
//! header byte ends the list. A file in more than one record has a run
//! list in each, for its own VCNs; [`Runs::add`] puts them together.

use radix_vfs::mem::{self, KVec};
use radix_vfs::{Error, Result};

/// The LCN of a run that is a hole.
pub const HOLE: u64 = u64::MAX;

/// A run of an attribute's clusters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    /// Its first virtual cluster.
    pub vcn: u64,
    /// How many clusters it has.
    pub len: u64,
    /// Where it starts on the volume; [`HOLE`] for a hole.
    pub lcn: u64,
}

/// The runs of an attribute, in VCN order with none missing.
pub struct Runs {
    runs: KVec<Run>,
}

impl Default for Runs {
    fn default() -> Runs {
        Runs::new()
    }
}

impl Runs {
    /// No runs.
    pub fn new() -> Runs {
        Runs { runs: mem::vec() }
    }

    /// The clusters the runs cover.
    pub fn clusters(&self) -> u64 {
        self.runs.last().map_or(0, |run| run.vcn + run.len)
    }

    /// The runs.
    pub fn as_slice(&self) -> &[Run] {
        &self.runs
    }

    /// Adds the runs of the packed list `bytes`, the piece of an attribute
    /// from VCN `lowest` to `highest`, on a volume of `clusters` clusters.
    /// The piece must start where the runs so far end. Fails with
    /// [`Error::Corrupt`] for a list that does not, that runs past the
    /// volume, or that does not end where its attribute says.
    pub fn add(&mut self, bytes: &[u8], lowest: u64, highest: u64, clusters: u64) -> Result<()> {
        if lowest != self.clusters() {
            return Err(Error::Corrupt);
        }
        let mut vcn = lowest;
        // Each piece's offsets start from 0 again.
        let mut lcn: i64 = 0;
        let mut at = 0;
        loop {
            let Some(&header) = bytes.get(at) else { return Err(Error::Corrupt) };
            if header == 0 {
                break;
            }
            let len_size = usize::from(header & 0xf);
            let off_size = usize::from(header >> 4);
            if len_size == 0 || len_size > 8 || off_size > 8 {
                return Err(Error::Corrupt);
            }
            let field = bytes.get(at + 1..at + 1 + len_size + off_size).ok_or(Error::Corrupt)?;
            let len = unsigned(&field[..len_size]);
            if len == 0 || len as i64 <= 0 {
                return Err(Error::Corrupt);
            }
            let start = if off_size == 0 {
                HOLE
            } else {
                lcn = lcn.checked_add(signed(&field[len_size..])).ok_or(Error::Corrupt)?;
                let start = u64::try_from(lcn).map_err(|_| Error::Corrupt)?;
                if start.checked_add(len).is_none_or(|end| end > clusters) {
                    return Err(Error::Corrupt);
                }
                start
            };
            self.push(Run { vcn, len, lcn: start })?;
            vcn = vcn.checked_add(len).ok_or(Error::Corrupt)?;
            at += 1 + len_size + off_size;
        }
        // An empty attribute's piece ends at VCN -1.
        if vcn != highest.wrapping_add(1) {
            return Err(Error::Corrupt);
        }
        Ok(())
    }

    /// Adds `run`, joining it to the last if it follows it on the disk.
    fn push(&mut self, run: Run) -> Result<()> {
        if let Some(last) = self.runs.last_mut()
            && last.vcn + last.len == run.vcn
            && (if last.lcn == HOLE {
                run.lcn == HOLE
            } else {
                run.lcn != HOLE && last.lcn + last.len == run.lcn
            })
        {
            last.len += run.len;
            return Ok(());
        }
        Ok(mem::push(&mut self.runs, run)?)
    }

    /// The run that holds VCN `vcn`, if any.
    pub fn find(&self, vcn: u64) -> Option<&Run> {
        let i = self.runs.partition_point(|run| run.vcn + run.len <= vcn);
        self.runs.get(i).filter(|run| run.vcn <= vcn)
    }

    /// Where VCN `vcn` is, and how many clusters from it on lie in a row
    /// there: its LCN, or [`HOLE`]. None past the runs.
    pub fn map(&self, vcn: u64) -> Option<(u64, u64)> {
        let run = self.find(vcn)?;
        let skip = vcn - run.vcn;
        let lcn = if run.lcn == HOLE { HOLE } else { run.lcn + skip };
        Some((lcn, run.len - skip))
    }

    /// Adds `len` clusters from LCN `lcn` (or a hole, [`HOLE`]) after the
    /// runs so far.
    pub fn append(&mut self, lcn: u64, len: u64) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        self.push(Run { vcn: self.clusters(), len, lcn })
    }

    /// Cuts the runs to their first `clusters` clusters; returns the runs
    /// cut off that were on the disk, whose clusters are free now.
    pub fn truncate(&mut self, clusters: u64) -> Result<KVec<Run>> {
        let mut freed = mem::vec();
        while let Some(last) = self.runs.last_mut() {
            if last.vcn >= clusters {
                let run = *last;
                self.runs.pop();
                if run.lcn != HOLE {
                    mem::push(&mut freed, run)?;
                }
            } else if last.vcn + last.len > clusters {
                let keep = clusters - last.vcn;
                if last.lcn != HOLE {
                    let cut = Run { vcn: clusters, len: last.len - keep, lcn: last.lcn + keep };
                    mem::push(&mut freed, cut)?;
                }
                last.len = keep;
                break;
            } else {
                break;
            }
        }
        Ok(freed)
    }

    /// Replaces clusters `vcn..vcn + len`, which the runs cover, with
    /// `with`: runs of `(lcn, clusters)` ([`HOLE`] for a hole) that cover
    /// `len` clusters between them. Returns those of the old runs there
    /// that were on the disk, whose clusters are free now.
    pub fn replace(&mut self, vcn: u64, len: u64, with: &[(u64, u64)]) -> Result<KVec<Run>> {
        let end = vcn.checked_add(len).ok_or(Error::Corrupt)?;
        if end > self.clusters() || with.iter().map(|&(_, n)| n).sum::<u64>() != len {
            return Err(Error::Corrupt);
        }
        let mut out = Runs { runs: mem::vec_with_capacity(self.runs.len() + with.len() + 1)? };
        let mut freed = mem::vec();
        for run in &self.runs {
            if run.vcn >= vcn {
                break;
            }
            out.push(Run { vcn: run.vcn, len: run.len.min(vcn - run.vcn), lcn: run.lcn })?;
        }
        let mut at = vcn;
        for &(lcn, n) in with {
            if n != 0 {
                out.push(Run { vcn: at, len: n, lcn })?;
                at += n;
            }
        }
        for run in &self.runs {
            let run_end = run.vcn + run.len;
            let (from, to) = (run.vcn.max(vcn), run_end.min(end));
            if from < to && run.lcn != HOLE {
                mem::push(&mut freed, Run { vcn: from, len: to - from, lcn: run.lcn + (from - run.vcn) })?;
            }
            if run_end > end {
                let skip = end.saturating_sub(run.vcn);
                let lcn = if run.lcn == HOLE { HOLE } else { run.lcn + skip };
                out.push(Run { vcn: run.vcn + skip, len: run.len - skip, lcn })?;
            }
        }
        *self = out;
        Ok(freed)
    }

    /// The packed run list of the runs from VCN `from` to `to` (not
    /// included), which must start and end on runs' edges or within
    /// them, as a record holds it, with its end byte.
    pub fn pack(&self, from: u64, to: u64) -> Result<KVec<u8>> {
        let mut out = mem::vec();
        let mut last_lcn: i64 = 0;
        for run in &self.runs {
            let start = run.vcn.max(from);
            let end = (run.vcn + run.len).min(to);
            if start >= end {
                continue;
            }
            let len = end - start;
            let len_bytes = signed_size(len as i64);
            let mut field = [0u8; 17];
            field[1..=len_bytes].copy_from_slice(&len.to_le_bytes()[..len_bytes]);
            let mut n = 1 + len_bytes;
            let off_bytes = if run.lcn == HOLE {
                0
            } else {
                let lcn = (run.lcn + (start - run.vcn)) as i64;
                let delta = lcn - last_lcn;
                last_lcn = lcn;
                let size = signed_size(delta);
                field[n..n + size].copy_from_slice(&delta.to_le_bytes()[..size]);
                n += size;
                size
            };
            field[0] = ((off_bytes as u8) << 4) | len_bytes as u8;
            mem::extend(&mut out, &field[..n])?;
        }
        mem::push(&mut out, 0)?;
        Ok(out)
    }

    /// How many of the clusters from `vcn`, `len` of them at most, are on
    /// the disk (not holes).
    pub fn allocated_in(&self, vcn: u64, len: u64) -> u64 {
        let mut at = vcn;
        let mut found = 0;
        while at < vcn + len {
            let Some((lcn, n)) = self.map(at) else { break };
            let n = n.min(vcn + len - at);
            if lcn != HOLE {
                found += n;
            }
            at += n;
        }
        found
    }
}

/// The fewest bytes that hold `n`, signed: its top bit must be its sign.
fn signed_size(n: i64) -> usize {
    (1..=8)
        .find(|&size| {
            let bits = 8 * size as u32;
            bits == 64 || (-(1i64 << (bits - 1))..(1i64 << (bits - 1))).contains(&n)
        })
        .unwrap_or(8)
}

/// The little-endian unsigned number in `bytes`.
fn unsigned(bytes: &[u8]) -> u64 {
    bytes.iter().rev().fold(0, |n, &b| (n << 8) | u64::from(b))
}

/// The little-endian signed number in `bytes`, of 1 to 8 bytes.
fn signed(bytes: &[u8]) -> i64 {
    let n = unsigned(bytes);
    let bits = 8 * bytes.len() as u32;
    if bits == 64 {
        n as i64
    } else {
        // Sign-extend from the top bit of the last byte.
        let shift = 64 - bits;
        ((n << shift) as i64) >> shift
    }
}
