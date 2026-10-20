//! Kernel address space layout randomization (KASLR).
//!
//! At every boot the kernel picks a random place for three things, so an
//! attacker cannot rely on knowing where kernel code or data lives:
//!
//! - the kernel image, moved by a *slide* of a whole number of 2 MiB pages
//!   within the top GiB of the address space (about 500 places, 9 bits);
//! - the direct map of physical memory, on a 1 GiB boundary in the first
//!   64 TiB of the higher half (up to 16 bits, fewer on machines with very
//!   wide physical addresses);
//! - the mapping window for stacks and device memory, on a 1 GiB boundary
//!   in the next 32 TiB (15 bits).
//!
//! Moving the image is the delicate part. The boot trampoline
//! (`boot.rs`) calls [`radix_kaslr_choose`] at the address the kernel was
//! linked at, maps the image a second time `slide` bytes higher, calls
//! [`radix_kaslr_relocate`] (still at the link address) to add the slide
//! to every absolute address in the image, and then enters the kernel at
//! its new address. During the relocation some addresses are moved and
//! some are not yet, but both copies of the image are mapped, so either
//! kind still works. The table of addresses to move comes from `ob build`
//! (`radix-relocs`). Both functions run before the image has moved, so
//! they store no pointers and cannot panic.
//!
//! The `nokaslr` flag turns all of it off, which `ob run --gdb` uses so
//! that the debugger's symbols match.

use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use radix_bootinfo::BootInfo;
use radix_hal::cmdline::CommandLine;
use radix_relocs::{Kind, Table};

use crate::cpu;

/// Granularity of the slide: one large page.
pub const STEP: u64 = 2 << 20;
/// The image and its moved copy are both mapped by one page directory.
const WINDOW: u64 = 1 << 30;

/// Whether and why the kernel moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// KASLR is on.
    On,
    /// The `nokaslr` flag turned it off.
    Disabled,
    /// The image has no relocation table (it was not built by `ob`).
    NoTable,
    /// The image is too large to move within its window.
    NoRoom,
}

static STATUS: AtomicU8 = AtomicU8::new(Status::NoTable as u8);
/// How many places the image could have gone to.
static PLACES: AtomicU64 = AtomicU64::new(1);
/// How many locations the relocation moved.
static MOVED: AtomicU64 = AtomicU64::new(0);
static SLIDE: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" {
    static __krelocs_start: u8;
    static __krelocs_end: u8;
    static __kernel_data_end: u8;
}

fn table() -> Option<Table<'static>> {
    // SAFETY: the linker script places `.krelocs` between these symbols in
    // the read-only part of the image; `ob build` fills it, and nothing
    // writes it at run time.
    let bytes = unsafe {
        let start = &raw const __krelocs_start;
        let len = (&raw const __krelocs_end as usize) - (start as usize);
        core::slice::from_raw_parts(start, len)
    };
    Table::parse(bytes)
}

/// Whether the command line in the boot information at `info_phys` (below
/// 4 GiB, mapped one to one) has `nokaslr`.
fn disabled(info_phys: u32) -> bool {
    if info_phys == 0 || !info_phys.is_multiple_of(8) {
        return false;
    }
    let address = info_phys as usize;
    // SAFETY: GRUB passed this address; the boot page tables map the first
    // 4 GiB at their physical addresses, and the structure starts with its
    // total size.
    let total = unsafe { (address as *const u32).read() } as usize;
    // SAFETY: as above, for the whole structure.
    let bytes = unsafe { core::slice::from_raw_parts(address as *const u8, total) };
    BootInfo::parse(bytes)
        .ok()
        .and_then(|info| info.command_line())
        .is_some_and(|line| CommandLine::new(line).has("nokaslr"))
}

/// Picks the slide. Called by the boot trampoline, at the link address,
/// with the physical address of the boot information; returns 0 to stay.
#[unsafe(no_mangle)]
extern "C" fn radix_kaslr_choose(info_phys: u32) -> u64 {
    let status = if disabled(info_phys) {
        Status::Disabled
    } else if table().is_none() {
        Status::NoTable
    } else {
        Status::On
    };
    STATUS.store(status as u8, Ordering::Relaxed);
    if status != Status::On {
        return 0;
    }
    // The original and the moved image must not overlap, and both must fit
    // in the window.
    // Still at the link address, so this is the image's physical end.
    let end = (&raw const __kernel_data_end as u64).wrapping_sub(crate::memory::KERNEL_OFFSET);
    let image = end.next_multiple_of(STEP);
    let first = image / STEP;
    let Some(places) = WINDOW.checked_sub(2 * image).map(|room| room / STEP + 1) else {
        STATUS.store(Status::NoRoom as u8, Ordering::Relaxed);
        return 0;
    };
    PLACES.store(places, Ordering::Relaxed);
    (first + cpu::random_u64() % places) * STEP
}

/// Adds `slide` to every location the relocation table lists. Called by
/// the boot trampoline, at the link address, with both copies of the image
/// mapped.
#[unsafe(no_mangle)]
extern "C" fn radix_kaslr_relocate(slide: u64) {
    let Some(table) = table() else { return };
    let mut moved = 0;
    for (kind, phys) in table.locations() {
        let address = phys as usize;
        match kind {
            Kind::Abs64 => {
                let slot = address as *mut u64;
                // SAFETY: `ob build` listed this location as an absolute
                // address inside the loaded image, below 4 GiB, where the
                // boot page tables map memory one to one and writably.
                unsafe {
                    slot.write_unaligned(radix_relocs::slide64(slot.read_unaligned(), slide))
                };
                moved += 1;
            }
            Kind::Abs32 => {
                let slot = address as *mut u32;
                // SAFETY: as above, for a 32-bit location.
                let value = unsafe { slot.read_unaligned() };
                if let Some(value) = radix_relocs::slide32(value, slide) {
                    // SAFETY: as above.
                    unsafe { slot.write_unaligned(value) };
                    moved += 1;
                }
            }
        }
    }
    MOVED.store(moved, Ordering::Relaxed);
}

/// Records the slide the trampoline used. Called first thing on the boot
/// CPU, at the new address.
pub fn set_slide(slide: u64) {
    SLIDE.store(slide, Ordering::Relaxed);
}

/// How far the image moved.
pub fn slide() -> u64 {
    SLIDE.load(Ordering::Relaxed)
}

/// Whether KASLR is on.
pub fn status() -> Status {
    match STATUS.load(Ordering::Relaxed) {
        0 => Status::On,
        1 => Status::Disabled,
        2 => Status::NoTable,
        _ => Status::NoRoom,
    }
}

/// How many places the image could have gone to, and how many locations
/// moved with it.
pub fn statistics() -> (u64, u64) {
    (PLACES.load(Ordering::Relaxed), MOVED.load(Ordering::Relaxed))
}
