//! Turning code addresses into function names, for backtraces and `kdb`.

use core::fmt;

use radix_hal::{Cpu, Platform};
use radix_ksyms::{Location, Symbols};

use crate::Arch;

/// The kernel's symbol table, if `ob build` filled it in.
pub fn table() -> Option<Symbols<'static>> {
    Symbols::parse(Arch::kernel_symbols())
}

/// The function containing `address`.
pub fn lookup(address: usize) -> Option<Location<'static>> {
    table()?.lookup(address as u64)
}

/// The function a return address belongs to. A return address points after
/// the call, which may be past the end of the calling function when the
/// call does not return, so the byte before it is looked up.
pub fn lookup_return(address: usize) -> Option<Location<'static>> {
    let location = lookup(address.checked_sub(1)?)?;
    Some(Location { name: location.name, offset: location.offset + 1 })
}

/// Most frames a backtrace shows.
pub const MAX_FRAMES: usize = 32;

/// A captured backtrace: the faulting address, if any, then return
/// addresses.
#[derive(Clone, Copy)]
pub struct Backtrace {
    frames: [usize; MAX_FRAMES],
    len: usize,
    /// The first frame is a faulting instruction, not a return address.
    exact_first: bool,
}

impl Backtrace {
    /// The backtrace of a fault: its instruction, then its callers.
    pub fn from_fault(pc: usize, frame_pointer: usize) -> Backtrace {
        let mut frames = [0; MAX_FRAMES];
        frames[0] = pc;
        let len = 1 + Arch::backtrace(frame_pointer, &mut frames[1..]);
        Backtrace { frames, len, exact_first: true }
    }

    /// The backtrace of the caller.
    #[inline(always)]
    pub fn here() -> Backtrace {
        let mut frames = [0; MAX_FRAMES];
        let len = Arch::backtrace(Arch::frame_pointer(), &mut frames);
        Backtrace { frames, len, exact_first: false }
    }

    /// Calls `line` once per frame with its formatted description.
    pub fn write(&self, line: &mut dyn FnMut(fmt::Arguments<'_>)) {
        if self.len == 0 {
            line(format_args!("  (no frames: the stack could not be walked)"));
            return;
        }
        let table = table();
        for (i, &address) in self.frames[..self.len].iter().enumerate() {
            let location = if i == 0 && self.exact_first {
                table.and_then(|t| t.lookup(address as u64))
            } else {
                lookup_return(address)
            };
            match (location, table) {
                (Some(location), _) => line(format_args!("  #{i:<2} {address:#018x}  {location}")),
                (None, Some(_)) => line(format_args!("  #{i:<2} {address:#018x}  ?")),
                (None, None) => {
                    line(format_args!("  #{i:<2} {address:#018x}  (no symbol table in this image)"))
                }
            }
        }
    }
}
