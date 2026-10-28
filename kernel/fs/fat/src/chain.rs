// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A file's cluster chain, held in memory.
//!
//! The table links a file's clusters one by one, so finding the cluster
//! that holds byte `n` means following the links from the first. A file
//! or directory loads its chain once, when its contents are first read
//! or written, and keeps it as runs of consecutive clusters: a file
//! written in one go is usually one run, so the chain stays small and
//! finding a cluster is a binary search.

use radix_vfs::mem::{self, KVec, NoMemory};

/// Clusters `cluster..cluster + count`, which hold the chain's clusters
/// from number `logical` on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Run {
    logical: u32,
    cluster: u32,
    count: u32,
}

/// The clusters of a file, in order.
pub struct Chain {
    runs: KVec<Run>,
    len: u32,
}

impl Default for Chain {
    fn default() -> Self {
        Chain::new()
    }
}

impl Chain {
    /// No clusters.
    pub fn new() -> Chain {
        Chain { runs: mem::vec(), len: 0 }
    }

    /// How many clusters there are.
    pub fn len(&self) -> u32 {
        self.len
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Cluster number `i` of the chain, and how many clusters from it on
    /// follow it on the disk without a gap (at least 1).
    pub fn extent(&self, i: u32) -> Option<(u32, u32)> {
        if i >= self.len {
            return None;
        }
        let at = self.runs.partition_point(|run| run.logical + run.count <= i);
        let run = self.runs.get(at)?;
        let into = i - run.logical;
        Some((run.cluster + into, run.count - into))
    }

    /// Cluster number `i`.
    pub fn get(&self, i: u32) -> Option<u32> {
        self.extent(i).map(|(cluster, _)| cluster)
    }

    /// The last cluster.
    pub fn last(&self) -> Option<u32> {
        self.runs.last().map(|run| run.cluster + run.count - 1)
    }

    /// Makes room to [`push`](Chain::push) one more cluster, so that it
    /// cannot fail.
    pub fn reserve(&mut self) -> Result<(), NoMemory> {
        self.runs.try_reserve(1).map_err(|_| NoMemory)
    }

    /// Adds `cluster` at the end.
    pub fn push(&mut self, cluster: u32) -> Result<(), NoMemory> {
        let len = self.len;
        match self.runs.last_mut() {
            Some(run) if run.cluster.checked_add(run.count) == Some(cluster) => run.count += 1,
            _ => mem::push(&mut self.runs, Run { logical: len, cluster, count: 1 })?,
        }
        self.len += 1;
        Ok(())
    }

    /// Adds the `count` clusters from `cluster` on at the end, in one go:
    /// exFAT's files that need no table are one such run.
    pub fn push_run(&mut self, cluster: u32, count: u32) -> Result<(), NoMemory> {
        if count == 0 {
            return Ok(());
        }
        let len = self.len;
        match self.runs.last_mut() {
            Some(run) if run.cluster.checked_add(run.count) == Some(cluster) => run.count += count,
            _ => mem::push(&mut self.runs, Run { logical: len, cluster, count })?,
        }
        self.len += count;
        Ok(())
    }

    /// Keeps the first `len` clusters.
    pub fn truncate(&mut self, len: u32) {
        if len >= self.len {
            return;
        }
        let keep = self.runs.partition_point(|run| run.logical < len);
        self.runs.truncate(keep);
        if let Some(run) = self.runs.last_mut() {
            run.count = run.count.min(len - run.logical);
        }
        self.len = len;
    }

    /// The clusters from number `from` on, in order.
    pub fn clusters_from(&self, from: u32) -> impl Iterator<Item = u32> + '_ {
        self.runs.iter().flat_map(move |run| {
            let skip = from.saturating_sub(run.logical).min(run.count);
            (run.cluster + skip)..(run.cluster + run.count)
        })
    }
}
