// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Extents compressed with zstd as they are written, on a mount that
//! compresses (ADR 0134).
//!
//! A file's data goes to extents of up to 128 KiB, each compressed whole
//! into one frame ([`crate::zstd::encode`]), as Linux writes them. An
//! extent whose frame would not save a sector is written as it is, as on
//! Linux; so is a file without checksums, which Linux never compresses.
//! The checksums of a compressed extent are of its sectors as written.
//!
//! A filesystem has two [`Packer`]s, made at its first compressed write
//! and dropped when memory runs short: writes compress two extents at a
//! time, the second on another CPU when one is free (`radix_vfs::join`),
//! before they take the transaction, so that reads go on meanwhile.

use radix_block::mem::CachePage;
use radix_block::{PAGE_SIZE, Page};
use radix_vfs::Result;
use radix_vfs::mem::{self, KBox, KVec};

use crate::unpack::{MAX_COMPRESSED, MAX_UNCOMPRESSED};
use crate::zstd::encode::Encoder;

/// The most pages an extent compressed holds.
pub(crate) const CHUNK: usize = MAX_UNCOMPRESSED / PAGE_SIZE;

/// An encoder and the pages a frame is written from.
pub(crate) struct Packer {
    encoder: KBox<Encoder>,
    /// The frame, as the encoder makes it, with room for its last words.
    frame: KVec<u8>,
    /// The frame, a sector a page, zeros after its end.
    pages: KVec<CachePage>,
}

impl Packer {
    /// A new packer, on the heap, in a frame of its own.
    #[inline(never)]
    pub(crate) fn boxed() -> Result<KBox<Packer>> {
        let mut pages = mem::vec_with_capacity(MAX_COMPRESSED / PAGE_SIZE)?;
        for _ in 0..MAX_COMPRESSED / PAGE_SIZE {
            mem::push(&mut pages, CachePage::new()?)?;
        }
        Ok(mem::boxed(Packer {
            encoder: encoder()?,
            frame: crate::zeroed(MAX_COMPRESSED + 8)?,
            pages,
        })?)
    }

    /// Compresses the first `n` bytes of `pages`, at most a chunk's, if
    /// that saves a sector of those the bytes fill, as Linux has it;
    /// returns how many bytes the frame takes.
    pub(crate) fn pack(&mut self, pages: &[&Page], n: usize) -> Option<usize> {
        let n = n.min(pages.len() * PAGE_SIZE).min(MAX_UNCOMPRESSED);
        let limit = n.div_ceil(PAGE_SIZE).saturating_sub(1) * PAGE_SIZE;
        if limit == 0 {
            return None;
        }
        let input = self.encoder.input();
        for (page, to) in pages.iter().zip(input[..n].chunks_mut(PAGE_SIZE)) {
            to.copy_from_slice(&page.bytes()[..to.len()]);
        }
        let made = self.encoder.compress(n, &mut self.frame, limit)?;
        for (page, from) in self.pages.iter_mut().zip(self.frame[..made].chunks(PAGE_SIZE)) {
            let bytes = page.bytes_mut();
            bytes[..from.len()].copy_from_slice(from);
            bytes[from.len()..].fill(0);
        }
        Some(made)
    }

    /// The pages holding the frame of `made` bytes.
    pub(crate) fn pages(&self, made: usize) -> impl Iterator<Item = &Page> {
        self.pages.iter().take(made.div_ceil(PAGE_SIZE)).map(|page| &**page)
    }

    /// The pages of memory it holds, about.
    pub(crate) fn held(&self) -> usize {
        (self.encoder.bytes() + self.frame.capacity()).div_ceil(PAGE_SIZE) + self.pages.len()
    }
}

/// An encoder, on the heap, made in a frame of its own: it is too big to
/// be moved through the packer's.
#[inline(never)]
fn encoder() -> Result<KBox<Encoder>> {
    Ok(mem::boxed(Encoder::new()?)?)
}
