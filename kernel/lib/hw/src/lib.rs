// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Device access for the Radix kernel: the plan's `lib/hw`.
//!
//! This is the one crate that touches device registers and the memory
//! devices read and write by themselves (DMA). It hands both out as safe
//! types, never as raw pointers, so the drivers built on it (the
//! compartments: `#![forbid(unsafe_code)]` crates such as `radix-virtio`)
//! cannot write anywhere else:
//!
//! - [`Mmio`], a window of device registers, read and written with
//!   volatile accesses at offsets checked against its length;
//! - [`Dma`], physically contiguous memory a device reads and writes,
//!   reached the same way and given back when dropped;
//! - [`Page`], a page of RAM a device reads or writes for one request at
//!   a time, while a driver borrows it (the page cache's pages);
//! - [`MsiMessage`], the address and data a device writes to raise a
//!   message-signalled interrupt, which the architecture hands out with
//!   the vector it raises;
//! - [`pci`], PCI configuration space through the PCI Express enhanced
//!   configuration access mechanism (ECAM): finding functions, sizing
//!   their base address registers and setting up MSI and MSI-X.
//!
//! The architecture maps device memory uncached and allocates DMA memory
//! (`radix_hal::Devices`); only it calls the `unsafe` constructors, whose
//! contracts say what it promises. With the `fake` feature, both types
//! can instead be backed by host memory or by a simulated device, so that
//! drivers' tests run on the host.

#![cfg_attr(not(any(test, feature = "fake")), no_std)]
#![deny(unsafe_code)]

pub mod pci;

#[cfg(any(test, feature = "fake"))]
extern crate std;

use core::fmt;
use core::sync::atomic::{Ordering, fence};

#[cfg(any(test, feature = "fake"))]
use std::sync::Arc;

/// A simulated device's registers, for tests (feature `fake`).
#[cfg(any(test, feature = "fake"))]
pub trait Registers: Send + Sync {
    /// Reads `size` bytes (1, 2, 4 or 8) at `offset`.
    fn read(&self, offset: usize, size: usize) -> u64;
    /// Writes the low `size` bytes of `value` at `offset`.
    fn write(&self, offset: usize, size: usize, value: u64);
}

#[derive(Clone)]
enum Backing {
    /// Device memory mapped uncached at this kernel address.
    Mapped(usize),
    /// A simulated device, at this offset into its registers.
    #[cfg(any(test, feature = "fake"))]
    Fake(Arc<dyn Registers>, usize),
}

/// A window of device registers (memory-mapped I/O).
///
/// Every access is volatile and checked: an offset outside the window, or
/// not a multiple of the access's size, panics, because it can only come
/// from a bug in the driver (the driver checks offsets it reads from the
/// device itself before it uses them, with [`Mmio::window`]). Device
/// mappings last for the kernel's life, so copies of a window can be kept
/// freely.
#[derive(Clone)]
pub struct Mmio {
    backing: Backing,
    len: usize,
}

impl fmt::Debug for Mmio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.backing {
            Backing::Mapped(base) => write!(f, "Mmio({base:#x}, {:#x} bytes)", self.len),
            #[cfg(any(test, feature = "fake"))]
            Backing::Fake(_, offset) => {
                write!(f, "Mmio(fake + {offset:#x}, {:#x} bytes)", self.len)
            }
        }
    }
}

impl Mmio {
    /// The registers mapped at kernel address `base`, `len` bytes of them.
    ///
    /// # Safety
    ///
    /// `base..base + len` must be device memory mapped uncached and
    /// readable and writable for the rest of the kernel's life, used by
    /// nothing that treats it as ordinary memory.
    #[allow(unsafe_code)]
    pub unsafe fn new(base: usize, len: usize) -> Mmio {
        Mmio { backing: Backing::Mapped(base), len }
    }

    /// Registers simulated by `regs`, `len` bytes of them (for tests).
    #[cfg(any(test, feature = "fake"))]
    pub fn fake(regs: Arc<dyn Registers>, len: usize) -> Mmio {
        Mmio { backing: Backing::Fake(regs, 0), len }
    }

    /// The window's length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the window is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The `len` bytes from `offset`, if they are inside this window.
    pub fn window(&self, offset: usize, len: usize) -> Option<Mmio> {
        let end = offset.checked_add(len)?;
        if end > self.len {
            return None;
        }
        let backing = match &self.backing {
            Backing::Mapped(base) => Backing::Mapped(base + offset),
            #[cfg(any(test, feature = "fake"))]
            Backing::Fake(regs, start) => Backing::Fake(regs.clone(), start + offset),
        };
        Some(Mmio { backing, len })
    }

    /// The address of `size` bytes at `offset`, after the checks.
    fn at(&self, offset: usize, size: usize) -> usize {
        assert!(
            offset.is_multiple_of(size)
                && offset.checked_add(size).is_some_and(|end| end <= self.len),
            "register access of {size} bytes at {offset:#x} outside {self:?}"
        );
        offset
    }

    /// Reads the register of `SIZE` bytes at `offset`.
    #[allow(unsafe_code)]
    fn read<T: Copy, const SIZE: usize>(&self, offset: usize, widen: fn(T) -> u64) -> u64 {
        let offset = self.at(offset, SIZE);
        match &self.backing {
            Backing::Mapped(base) => {
                // SAFETY: `new`'s contract makes the window device memory
                // mapped for the kernel's life, and `at` kept the access
                // inside it and aligned.
                let value = unsafe { core::ptr::read_volatile((base + offset) as *const T) };
                widen(value)
            }
            #[cfg(any(test, feature = "fake"))]
            Backing::Fake(regs, start) => regs.read(start + offset, SIZE),
        }
    }

    /// Writes the register of `SIZE` bytes at `offset`.
    #[allow(unsafe_code)]
    fn write<T: Copy, const SIZE: usize>(&self, offset: usize, value: T, wide: u64) {
        let offset = self.at(offset, SIZE);
        match &self.backing {
            Backing::Mapped(base) => {
                let _ = wide;
                // SAFETY: as in `read`.
                unsafe { core::ptr::write_volatile((base + offset) as *mut T, value) }
            }
            #[cfg(any(test, feature = "fake"))]
            Backing::Fake(regs, start) => {
                let _ = value;
                regs.write(start + offset, SIZE, wide)
            }
        }
    }

    /// Reads the byte register at `offset`.
    pub fn read8(&self, offset: usize) -> u8 {
        self.read::<u8, 1>(offset, u64::from) as u8
    }

    /// Reads the 16-bit register at `offset`.
    pub fn read16(&self, offset: usize) -> u16 {
        self.read::<u16, 2>(offset, u64::from) as u16
    }

    /// Reads the 32-bit register at `offset`.
    pub fn read32(&self, offset: usize) -> u32 {
        self.read::<u32, 4>(offset, u64::from) as u32
    }

    /// Reads the 64-bit register at `offset`.
    pub fn read64(&self, offset: usize) -> u64 {
        self.read::<u64, 8>(offset, |v| v)
    }

    /// Writes the byte register at `offset`.
    pub fn write8(&self, offset: usize, value: u8) {
        self.write::<u8, 1>(offset, value, u64::from(value));
    }

    /// Writes the 16-bit register at `offset`.
    pub fn write16(&self, offset: usize, value: u16) {
        self.write::<u16, 2>(offset, value, u64::from(value));
    }

    /// Writes the 32-bit register at `offset`.
    pub fn write32(&self, offset: usize, value: u32) {
        self.write::<u32, 4>(offset, value, u64::from(value));
    }

    /// Writes the 64-bit register at `offset`.
    pub fn write64(&self, offset: usize, value: u64) {
        self.write::<u64, 8>(offset, value, value);
    }
}

/// Gives back DMA memory: called with its kernel address, physical address
/// and length when a [`Dma`] is dropped.
pub type Release = fn(virt: usize, phys: u64, len: usize);

/// Physically contiguous memory that a device reads and writes by itself.
///
/// The device may change it at any time it owns a part, so the CPU reaches
/// it only with volatile accesses at checked offsets, like registers; a
/// driver orders its writes before telling the device with
/// [`device_barrier`]. It is given back when dropped, so a driver must not
/// drop it while the device may still use it (resetting the device first).
pub struct Dma {
    virt: usize,
    phys: u64,
    len: usize,
    release: Release,
}

impl fmt::Debug for Dma {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Dma({:#x} at {:#x}, {:#x} bytes)", self.virt, self.phys, self.len)
    }
}

impl Dma {
    /// The memory at kernel address `virt` and physical address `phys`,
    /// `len` bytes, given back with `release`.
    ///
    /// # Safety
    ///
    /// The memory must be mapped readable and writable at `virt`, be
    /// physically contiguous from `phys`, and belong to the new value
    /// alone until `release` gives it back.
    #[allow(unsafe_code)]
    pub unsafe fn new(virt: usize, phys: u64, len: usize, release: Release) -> Dma {
        Dma { virt, phys, len, release }
    }

    /// `len` zeroed bytes of host memory, whose physical address is its
    /// host address, so a simulated device can reach it with
    /// [`fake_memory`] (for tests).
    #[cfg(any(test, feature = "fake"))]
    pub fn fake(len: usize) -> Dma {
        let pages = len.div_ceil(4096).max(1);
        let memory: &'static mut [u64] = std::vec![0u64; pages * 512].leak();
        let virt = memory.as_mut_ptr() as usize;
        fn release(virt: usize, _phys: u64, len: usize) {
            let pages = len.div_ceil(4096).max(1);
            // SAFETY: `fake` leaked exactly this vector.
            #[allow(unsafe_code)]
            drop(unsafe {
                std::vec::Vec::from_raw_parts(virt as *mut u64, pages * 512, pages * 512)
            });
        }
        Dma { virt, phys: virt as u64, len, release }
    }

    /// The physical address of its first byte, which the device uses.
    pub fn phys(&self) -> u64 {
        self.phys
    }

    /// Its length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether it is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn at(&self, offset: usize, size: usize) -> usize {
        assert!(
            offset.checked_add(size).is_some_and(|end| end <= self.len),
            "DMA access of {size} bytes at {offset:#x} outside {self:?}"
        );
        self.virt + offset
    }

    /// Reads the `u16` at `offset` (aligned).
    #[allow(unsafe_code)]
    pub fn read16(&self, offset: usize) -> u16 {
        assert!(offset.is_multiple_of(2));
        // SAFETY: `new`'s contract makes the memory ours and mapped, and
        // `at` kept the access inside it; the access is aligned.
        unsafe { core::ptr::read_volatile(self.at(offset, 2) as *const u16) }
    }

    /// Reads the `u32` at `offset` (aligned).
    #[allow(unsafe_code)]
    pub fn read32(&self, offset: usize) -> u32 {
        assert!(offset.is_multiple_of(4));
        // SAFETY: as in `read16`.
        unsafe { core::ptr::read_volatile(self.at(offset, 4) as *const u32) }
    }

    /// Reads the `u64` at `offset` (aligned).
    #[allow(unsafe_code)]
    pub fn read64(&self, offset: usize) -> u64 {
        assert!(offset.is_multiple_of(8));
        // SAFETY: as in `read16`.
        unsafe { core::ptr::read_volatile(self.at(offset, 8) as *const u64) }
    }

    /// Reads the byte at `offset`.
    #[allow(unsafe_code)]
    pub fn read8(&self, offset: usize) -> u8 {
        // SAFETY: as in `read16`.
        unsafe { core::ptr::read_volatile(self.at(offset, 1) as *const u8) }
    }

    /// Writes the byte at `offset`.
    #[allow(unsafe_code)]
    pub fn write8(&self, offset: usize, value: u8) {
        // SAFETY: as in `read16`.
        unsafe { core::ptr::write_volatile(self.at(offset, 1) as *mut u8, value) }
    }

    /// Writes the `u16` at `offset` (aligned).
    #[allow(unsafe_code)]
    pub fn write16(&self, offset: usize, value: u16) {
        assert!(offset.is_multiple_of(2));
        // SAFETY: as in `read16`.
        unsafe { core::ptr::write_volatile(self.at(offset, 2) as *mut u16, value) }
    }

    /// Writes the `u32` at `offset` (aligned).
    #[allow(unsafe_code)]
    pub fn write32(&self, offset: usize, value: u32) {
        assert!(offset.is_multiple_of(4));
        // SAFETY: as in `read16`.
        unsafe { core::ptr::write_volatile(self.at(offset, 4) as *mut u32, value) }
    }

    /// Writes the `u64` at `offset` (aligned).
    #[allow(unsafe_code)]
    pub fn write64(&self, offset: usize, value: u64) {
        assert!(offset.is_multiple_of(8));
        // SAFETY: as in `read16`.
        unsafe { core::ptr::write_volatile(self.at(offset, 8) as *mut u64, value) }
    }

    /// Copies `out.len()` bytes from `offset` into `out`.
    pub fn read(&self, offset: usize, out: &mut [u8]) {
        self.at(offset, out.len());
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = self.read8(offset + i);
        }
    }

    /// Copies `data` to `offset`.
    pub fn write(&self, offset: usize, data: &[u8]) {
        self.at(offset, data.len());
        for (i, &byte) in data.iter().enumerate() {
            self.write8(offset + i, byte);
        }
    }

    /// Zeroes all of it.
    pub fn zero(&self) {
        for offset in (0..self.len & !7).step_by(8) {
            self.write64(offset, 0);
        }
        for offset in self.len & !7..self.len {
            self.write8(offset, 0);
        }
    }
}

impl Drop for Dma {
    fn drop(&mut self) {
        (self.release)(self.virt, self.phys, self.len);
    }
}

/// The size of a [`Page`].
pub const PAGE_SIZE: usize = 4096;

/// Gives back a page: called with its kernel address and physical address
/// when a [`Page`] is dropped.
pub type PageRelease = fn(virt: usize, phys: u64);

/// A page of ordinary RAM that devices may read and write by DMA: a page
/// of the page cache, which a disk read fills and a disk write takes from.
///
/// Unlike [`Dma`], a device uses a page only while a driver holds a
/// borrow of it for one request: a mutable one while the device writes
/// into it (a read from the disk), a shared one while the device reads
/// from it. Block drivers take the borrow for the whole request and return
/// only once the device is done with the page, so the CPU never looks at
/// bytes a device is changing, and the page's bytes can be plain slices
/// ([`Page::bytes`], [`Page::bytes_mut`]), which copies to and from user
/// memory need. After a device wrote into it, the driver calls
/// [`device_wrote`] on its bytes before giving it back.
pub struct Page {
    virt: usize,
    phys: u64,
    release: PageRelease,
}

impl fmt::Debug for Page {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Page({:#x} at {:#x})", self.virt, self.phys)
    }
}

impl Page {
    /// The page at kernel address `virt` and physical address `phys`,
    /// given back with `release`.
    ///
    /// # Safety
    ///
    /// The [`PAGE_SIZE`] bytes from `virt` must be RAM, mapped readable
    /// and writable at `virt`, at physical address `phys`, and belong to
    /// the new value alone until `release` gives them back.
    #[allow(unsafe_code)]
    pub unsafe fn new(virt: usize, phys: u64, release: PageRelease) -> Page {
        Page { virt, phys, release }
    }

    /// A zeroed page of host memory, whose physical address is its host
    /// address (for tests).
    #[cfg(any(test, feature = "fake"))]
    pub fn fake() -> Page {
        let memory: &'static mut [u64] = std::vec![0u64; PAGE_SIZE / 8].leak();
        let virt = memory.as_mut_ptr() as usize;
        fn release(virt: usize, _phys: u64) {
            // SAFETY: `fake` leaked exactly this vector.
            #[allow(unsafe_code)]
            drop(unsafe {
                std::vec::Vec::from_raw_parts(virt as *mut u64, PAGE_SIZE / 8, PAGE_SIZE / 8)
            });
        }
        Page { virt, phys: virt as u64, release }
    }

    /// The physical address of its first byte, which a device uses.
    pub fn phys(&self) -> u64 {
        self.phys
    }

    /// Its bytes.
    #[allow(unsafe_code)]
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `new`'s contract makes the page RAM that is ours and
        // mapped; a device writes into it only while a driver holds it
        // mutably borrowed, so not while this borrow lives.
        unsafe { core::slice::from_raw_parts(self.virt as *const u8, PAGE_SIZE) }
    }

    /// Its bytes, to change.
    #[allow(unsafe_code)]
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as in `bytes`; the mutable borrow of the page makes this
        // the only reference to its bytes.
        unsafe { core::slice::from_raw_parts_mut(self.virt as *mut u8, PAGE_SIZE) }
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        (self.release)(self.virt, self.phys);
    }
}

/// Reads `out.len()` bytes of host memory at the fake physical address
/// `phys`, as a simulated device does (feature `fake`).
///
/// # Panics
///
/// Never checks the address: it must come from a [`Dma::fake`] buffer or
/// a buffer the test lent, which the test keeps alive.
#[cfg(any(test, feature = "fake"))]
#[allow(unsafe_code)]
pub fn fake_memory(phys: u64, out: &mut [u8]) {
    // SAFETY: the caller passes an address of live host memory (see above).
    unsafe { core::ptr::copy_nonoverlapping(phys as *const u8, out.as_mut_ptr(), out.len()) }
}

/// Writes `data` to host memory at the fake physical address `phys`, as a
/// simulated device does (feature `fake`); see [`fake_memory`].
#[cfg(any(test, feature = "fake"))]
#[allow(unsafe_code)]
pub fn fake_memory_write(phys: u64, data: &[u8]) {
    // SAFETY: as in `fake_memory`.
    unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), phys as *mut u8, data.len()) }
}

/// Orders the CPU's earlier writes to DMA memory before its later accesses
/// to device registers, and the reverse: call it between filling in what
/// a device will read and telling the device (writing a doorbell
/// register), and between learning that the device is done and reading
/// what it wrote.
pub fn device_barrier() {
    fence(Ordering::SeqCst);
}

/// A buffer of ordinary memory a device has written into by DMA (a page
/// of the page cache that a disk read filled): tells the compiler that the
/// bytes may have changed behind its back, so it reads them again.
pub fn device_wrote(buffer: &mut [u8]) {
    device_barrier();
    core::hint::black_box(buffer);
}

/// What a device writes to raise a message-signalled interrupt (MSI or
/// MSI-X): `data` at `address`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsiMessage {
    /// The address the device writes to.
    pub address: u64,
    /// The value it writes.
    pub data: u32,
}

#[cfg(test)]
mod tests;
