// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The heap behind `alloc`: frames from the program's own memory object
//! (`SLOT_MEMORY`), mapped into its address space as it grows.
//!
//! Small blocks, up to 2 KiB, come in eight sizes, each a power of two
//! and aligned to its size. They are cut from chunks of 64 KiB, and a
//! freed block goes on a list for its size, for the next block of that
//! size. Larger blocks get frames of their own, which go back to the
//! kernel when they are freed. Chunks are never given back, and neither
//! is a freed large block's memory to the memory object, whose pages
//! only come back once everything made from it has gone (radix-memory(2)):
//! enough for the programs of Phase 6, and docs/design/processes.md
//! lists it.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

use libradix::SYS;
use radix_abi::address_space::PAGE_SIZE;
use radix_abi::capability::{self, ObjectKind, Rights};
use radix_abi::startup::{SLOT_MEMORY, SLOT_SPACE};
use radix_abi::{memory, syscalls};

use crate::lock::Lock;
use crate::{slots, vm};

/// The sizes of small blocks.
const SIZES: [usize; 8] = [16, 32, 64, 128, 256, 512, 1024, 2048];
/// A chunk small blocks are cut from: 2^order pages.
const CHUNK_ORDER: u32 = 4;
const CHUNK: usize = (PAGE_SIZE as usize) << CHUNK_ORDER;
/// The most large blocks in use at once.
const LARGE_MAX: usize = 64;

/// A free small block, linked to the next of its size.
struct Free {
    next: *mut Free,
}

/// A large block: where it is mapped, the slot of its frame, its pages.
#[derive(Clone, Copy)]
struct Large {
    address: usize,
    frame: u32,
    pages: u64,
}

struct Heap {
    /// The free blocks of each size.
    free: [*mut Free; SIZES.len()],
    /// What is left of the chunk being cut: from `cut` to `end`.
    cut: usize,
    end: usize,
    large: [Option<Large>; LARGE_MAX],
}

// SAFETY: the pointers are to the heap's own memory, which any thread of
// the program may use; the lock around the heap keeps them consistent.
unsafe impl Send for Heap {}

static HEAP: Lock<Heap> = Lock::new(Heap {
    free: [ptr::null_mut(); SIZES.len()],
    cut: 0,
    end: 0,
    large: [None; LARGE_MAX],
});

/// The allocator of every program built with the runtime.
pub struct Allocator;

#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

/// The size class of a block of `size` bytes aligned to `align`.
fn class_of(size: usize, align: usize) -> Option<usize> {
    let size = size.max(align);
    SIZES.iter().position(|&class| class >= size)
}

/// Makes a frame of 2^`order` pages and maps it read-write. Returns its
/// address and the slot of its capability.
fn map_frame(order: u32) -> Option<(usize, u32)> {
    let pages = 1u64 << order;
    let slot = slots::take()?;
    if memory::retype(&SYS, SLOT_MEMORY, ObjectKind::Frame, order, slot, 1).is_err() {
        slots::give_back(slot);
        return None;
    }
    let rights = Rights::READ.union(Rights::WRITE);
    let mapped = vm::take(pages, PAGE_SIZE)
        .filter(|&address| syscalls::map(&SYS, SLOT_SPACE, slot, address, rights, 0, pages).is_ok());
    match mapped {
        Some(address) => Some((address as usize, slot)),
        None => {
            let _ = capability::delete(&SYS, slot);
            slots::give_back(slot);
            None
        }
    }
}

impl Heap {
    fn small(&mut self, class: usize) -> *mut u8 {
        let block = self.free[class];
        if !block.is_null() {
            // SAFETY: blocks on the list are free blocks of this size,
            // each holding the link to the next.
            self.free[class] = unsafe { (*block).next };
            return block.cast();
        }
        let size = SIZES[class];
        let mut at = self.cut.next_multiple_of(size);
        if self.cut == 0 || at + size > self.end {
            // What is left of the old chunk is not used again.
            let Some((address, _)) = map_frame(CHUNK_ORDER) else { return ptr::null_mut() };
            (self.cut, self.end) = (address, address + CHUNK);
            at = address;
        }
        self.cut = at + size;
        at as *mut u8
    }

    fn free_small(&mut self, block: *mut u8, class: usize) {
        let block = block.cast::<Free>();
        // SAFETY: the block is a free block of this size class now, at
        // least 16 bytes and aligned to its size, so it holds a link.
        unsafe { block.write(Free { next: self.free[class] }) };
        self.free[class] = block;
    }

    fn large(&mut self, layout: Layout) -> *mut u8 {
        if layout.align() > PAGE_SIZE as usize {
            return ptr::null_mut();
        }
        let Some(entry) = self.large.iter().position(Option::is_none) else {
            return ptr::null_mut();
        };
        let pages = (layout.size() as u64).div_ceil(PAGE_SIZE).next_power_of_two();
        let Some((address, frame)) = map_frame(pages.trailing_zeros()) else {
            return ptr::null_mut();
        };
        self.large[entry] = Some(Large { address, frame, pages });
        address as *mut u8
    }

    fn free_large(&mut self, block: *mut u8) {
        let found = self.large.iter_mut().find(|e| e.is_some_and(|e| e.address == block as usize));
        if let Some(entry) = found.and_then(Option::take) {
            let _ = syscalls::unmap(&SYS, SLOT_SPACE, entry.address as u64, entry.pages);
            let _ = capability::delete(&SYS, entry.frame);
            slots::give_back(entry.frame);
        }
    }
}

// SAFETY: blocks are cut from memory mapped for nothing else, never
// handed out twice while in use, sized and aligned as asked (small ones
// to their class's size, which is at least the size and the alignment;
// large ones to a page, and larger alignments are refused).
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut heap = HEAP.lock();
        match class_of(layout.size(), layout.align()) {
            Some(class) => heap.small(class),
            None => heap.large(layout),
        }
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        let mut heap = HEAP.lock();
        match class_of(layout.size(), layout.align()) {
            Some(class) => heap.free_small(block, class),
            None => heap.free_large(block),
        }
    }
}
