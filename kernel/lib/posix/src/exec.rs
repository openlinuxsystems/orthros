// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What `exec` reads and writes besides the program itself: the block of
//! strings a caller passes ([`Block`]), a script's `#!` line
//! ([`shebang`]), and the initial stack the System V ABI gives a program
//! ([`Stack`]).
//!
//! The block is an ExecHeader (radix-process(2)) and then its strings,
//! each ending with a NUL: the arguments, the environment, and the extra
//! strings, of which the first is the working directory. The stack is laid
//! out as the x86-64 psABI and Linux lay it out, from its top down: the
//! strings, 16 random bytes, the platform's name, then (16-byte aligned)
//! `argc`, the argument pointers and a null, the environment pointers and
//! a null, and the auxiliary vector, ending with AT_NULL. A C library's
//! start-up code reads it as it would on Linux.

use radix_abi::process::{EXEC_MAX, EXEC_VERSION, ExecHeader};

/// The most strings a block may hold in all.
pub const STRINGS_MAX: u32 = 32768;

/// The longest `#!` line read, as Linux reads it.
pub const SHEBANG_MAX: usize = 256;

/// Why a block was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockError {
    /// Larger than EXEC_MAX, or holding more than [`STRINGS_MAX`] strings.
    TooLong,
    /// Its header or strings do not add up.
    Malformed,
}

/// A checked exec block.
#[derive(Clone, Copy, Debug)]
pub struct Block<'a> {
    /// The strings, after the header.
    strings: &'a [u8],
    /// How many arguments, environment strings and extra strings.
    counts: [u32; 3],
}

impl<'a> Block<'a> {
    /// Reads and checks the block in `bytes`: a header of the right
    /// version, then exactly as many NUL-terminated strings as it says,
    /// and nothing after them.
    pub fn read(bytes: &'a [u8]) -> Result<Block<'a>, BlockError> {
        if bytes.len() as u64 > EXEC_MAX {
            return Err(BlockError::TooLong);
        }
        let header = bytes.get(..ExecHeader::SIZE).ok_or(BlockError::Malformed)?;
        let word = |at: usize| u32::from_le_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]]);
        if word(ExecHeader::OFFSET_VERSION) != EXEC_VERSION {
            return Err(BlockError::Malformed);
        }
        let counts = [
            word(ExecHeader::OFFSET_ARGS),
            word(ExecHeader::OFFSET_ENV),
            word(ExecHeader::OFFSET_EXTRA),
        ];
        let total = counts.iter().try_fold(0u32, |sum, &n| sum.checked_add(n));
        let total = total.ok_or(BlockError::TooLong)?;
        if total > STRINGS_MAX {
            return Err(BlockError::TooLong);
        }
        let strings = &bytes[ExecHeader::SIZE..];
        let nuls = strings.iter().filter(|&&b| b == 0).count();
        if nuls != total as usize || strings.last().is_some_and(|&b| b != 0) {
            return Err(BlockError::Malformed);
        }
        Ok(Block { strings, counts })
    }

    /// A block of no strings at all.
    pub const fn empty() -> Block<'static> {
        Block { strings: &[], counts: [0; 3] }
    }

    /// Every string, in order, without its NUL.
    fn all(&self) -> impl Iterator<Item = &'a [u8]> + use<'a> {
        let strings = self.strings;
        let mut rest = if strings.is_empty() { None } else { Some(strings) };
        core::iter::from_fn(move || {
            let bytes = rest?;
            let end = bytes.iter().position(|&b| b == 0)?;
            let tail = &bytes[end + 1..];
            rest = if tail.is_empty() { None } else { Some(tail) };
            Some(&bytes[..end])
        })
    }

    /// The arguments.
    pub fn args(&self) -> impl Iterator<Item = &'a [u8]> + use<'a> {
        self.all().take(self.counts[0] as usize)
    }

    /// The environment.
    pub fn env(&self) -> impl Iterator<Item = &'a [u8]> + use<'a> {
        self.all().skip(self.counts[0] as usize).take(self.counts[1] as usize)
    }

    /// The extra strings: the working directory first.
    pub fn extra(&self) -> impl Iterator<Item = &'a [u8]> + use<'a> {
        self.all().skip((self.counts[0] + self.counts[1]) as usize)
    }

    /// How many arguments.
    pub const fn arg_count(&self) -> u32 {
        self.counts[0]
    }

    /// How many environment strings.
    pub const fn env_count(&self) -> u32 {
        self.counts[1]
    }
}

/// A script's `#!` line: the interpreter's path, and at most one argument
/// (the rest of the line, trimmed), as Linux reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shebang<'a> {
    /// The interpreter's path.
    pub interpreter: &'a [u8],
    /// Its argument, if the line has one.
    pub argument: Option<&'a [u8]>,
}

/// Reads the `#!` line at the start of `head` (the file's first bytes, up
/// to [`SHEBANG_MAX`] of them), if it has one that names an interpreter.
pub fn shebang(head: &[u8]) -> Option<Shebang<'_>> {
    let line = head.strip_prefix(b"#!")?;
    let line = &line[..line.len().min(SHEBANG_MAX - 2)];
    let end = line.iter().position(|&b| b == b'\n').unwrap_or(line.len());
    let blank = |b: &u8| *b == b' ' || *b == b'\t';
    let line = trim(&line[..end], blank);
    if line.is_empty() {
        return None;
    }
    let split = line.iter().position(blank).unwrap_or(line.len());
    let (interpreter, rest) = line.split_at(split);
    let rest = trim(rest, blank);
    Some(Shebang { interpreter, argument: (!rest.is_empty()).then_some(rest) })
}

fn trim(bytes: &[u8], blank: impl Fn(&u8) -> bool) -> &[u8] {
    let start = bytes.iter().position(|b| !blank(b)).unwrap_or(bytes.len());
    let end = bytes.iter().rposition(|b| !blank(b)).map_or(start, |end| end + 1);
    &bytes[start..end]
}

/// Auxiliary vector entries, as Linux numbers them.
pub mod aux {
    /// The end of the vector.
    pub const AT_NULL: u64 = 0;
    /// The program's program headers, in memory.
    pub const AT_PHDR: u64 = 3;
    /// The size of one program header.
    pub const AT_PHENT: u64 = 4;
    /// How many program headers.
    pub const AT_PHNUM: u64 = 5;
    /// The page size.
    pub const AT_PAGESZ: u64 = 6;
    /// Where the interpreter was loaded.
    pub const AT_BASE: u64 = 7;
    /// Flags (0).
    pub const AT_FLAGS: u64 = 8;
    /// The program's entry point.
    pub const AT_ENTRY: u64 = 9;
    /// User and group numbers (0 until Phase 13 brings users).
    pub const AT_UID: u64 = 11;
    /// See [`AT_UID`].
    pub const AT_EUID: u64 = 12;
    /// See [`AT_UID`].
    pub const AT_GID: u64 = 13;
    /// See [`AT_UID`].
    pub const AT_EGID: u64 = 14;
    /// The platform's name.
    pub const AT_PLATFORM: u64 = 15;
    /// CPU features (CPUID leaf 1's EDX).
    pub const AT_HWCAP: u64 = 16;
    /// The clock tick rate `times` counts in.
    pub const AT_CLKTCK: u64 = 17;
    /// Whether the program runs with more privilege than its starter (0).
    pub const AT_SECURE: u64 = 23;
    /// 16 random bytes.
    pub const AT_RANDOM: u64 = 25;
    /// The path the program was run as.
    pub const AT_EXECFN: u64 = 31;
    /// Radix's own (Phase 8): the working directory, a string the C
    /// library keeps, since the kernel keeps none.
    pub const AT_RADIX_CWD: u64 = 0x5244_0001;
    /// Radix's own: the start-up page (radix-startup(2)), so that
    /// programs built on radix-rt start the same way under `exec`.
    pub const AT_RADIX_STARTUP: u64 = 0x5244_0002;
}

/// The strings of an initial stack, and where the pointers to them go.
#[derive(Clone, Copy, Debug)]
pub struct Stack<'a> {
    /// The arguments.
    pub args: &'a [&'a [u8]],
    /// The environment.
    pub env: &'a [&'a [u8]],
    /// The auxiliary vector, but for the entries this adds itself
    /// (AT_RANDOM, AT_PLATFORM, AT_EXECFN, AT_RADIX_CWD) and AT_NULL.
    pub aux: &'a [(u64, u64)],
    /// The path the program was run as (AT_EXECFN).
    pub execfn: &'a [u8],
    /// The working directory (AT_RADIX_CWD), if there is one.
    pub cwd: Option<&'a [u8]>,
    /// The platform's name (AT_PLATFORM).
    pub platform: &'a [u8],
    /// The random bytes (AT_RANDOM).
    pub random: [u8; 16],
}

/// Why a stack could not be laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackError {
    /// It does not fit in the room given.
    TooBig,
}

impl Stack<'_> {
    /// How many auxiliary entries the stack holds, AT_NULL included.
    fn aux_entries(&self) -> usize {
        self.aux.len() + 3 + usize::from(self.cwd.is_some()) + 1
    }

    /// The bytes the argument, environment, AT_EXECFN and AT_RADIX_CWD
    /// strings take, each with its NUL.
    fn strings_len(&self) -> u64 {
        let sum = |list: &[&[u8]]| list.iter().map(|s| s.len() as u64 + 1).sum::<u64>();
        sum(self.args) + sum(self.env) + self.execfn.len() as u64 + 1
            + self.cwd.map_or(0, |cwd| cwd.len() as u64 + 1)
    }

    /// How many words the pointers and the auxiliary vector take, from
    /// `argc` to AT_NULL's value.
    fn words(&self) -> u64 {
        (1 + self.args.len() + 1 + self.env.len() + 1 + 2 * self.aux_entries()) as u64
    }

    /// The most bytes the stack takes below its top.
    pub fn size(&self) -> u64 {
        // The end marker, the strings, the platform's name, the random
        // bytes, up to 15 bytes of alignment, and the words.
        8 + self.strings_len() + self.platform.len() as u64 + 1 + 16 + 15 + 8 * self.words()
    }

    /// Lays the stack out under `top` (16-byte aligned) into `out`, which
    /// stands for the `out.len()` bytes just below `top`. From the top
    /// down, as Linux lays it out: 8 zero bytes, the strings (arguments
    /// lowest, then the environment, AT_EXECFN's and AT_RADIX_CWD's), the
    /// platform's name, the random bytes, and the words. Returns the stack
    /// pointer the program starts with, which points at `argc` and is
    /// 16-byte aligned; the bytes below it are left as they were.
    pub fn write(&self, top: u64, out: &mut [u8]) -> Result<u64, StackError> {
        let room = out.len() as u64;
        if !top.is_multiple_of(16) || top < room || self.size() > room {
            return Err(StackError::TooBig);
        }
        let base = top - room;
        let index = |address: u64| (address - base) as usize;
        let end = top - 8;
        out[index(end)..].fill(0);
        let start = end - self.strings_len();
        // The strings, upwards from `start`.
        let mut at = start;
        let mut place = |out: &mut [u8], bytes: &[u8]| {
            let i = index(at);
            out[i..i + bytes.len()].copy_from_slice(bytes);
            out[i + bytes.len()] = 0;
            let placed = at;
            at += bytes.len() as u64 + 1;
            placed
        };
        for arg in self.args {
            place(out, arg);
        }
        for var in self.env {
            place(out, var);
        }
        let execfn = place(out, self.execfn);
        let cwd = self.cwd.map(|cwd| place(out, cwd));
        let platform = start - (self.platform.len() as u64 + 1);
        out[index(platform)..index(platform) + self.platform.len()].copy_from_slice(self.platform);
        out[index(start) - 1] = 0;
        let random = platform - 16;
        out[index(random)..index(platform)].copy_from_slice(&self.random);
        let sp = (random - 8 * self.words()) & !15;
        // The words, upwards from the stack pointer, and zeros up to the
        // random bytes.
        let mut word_at = sp;
        let mut word = |out: &mut [u8], value: u64| {
            let i = index(word_at);
            out[i..i + 8].copy_from_slice(&value.to_le_bytes());
            word_at += 8;
        };
        word(out, self.args.len() as u64);
        let mut string = start;
        for arg in self.args {
            word(out, string);
            string += arg.len() as u64 + 1;
        }
        word(out, 0);
        for var in self.env {
            word(out, string);
            string += var.len() as u64 + 1;
        }
        word(out, 0);
        for &(key, value) in self.aux {
            word(out, key);
            word(out, value);
        }
        for (key, value) in [
            (aux::AT_RANDOM, random),
            (aux::AT_PLATFORM, platform),
            (aux::AT_EXECFN, execfn),
        ] {
            word(out, key);
            word(out, value);
        }
        if let Some(cwd) = cwd {
            word(out, aux::AT_RADIX_CWD);
            word(out, cwd);
        }
        word(out, aux::AT_NULL);
        word(out, 0);
        out[index(word_at)..index(random)].fill(0);
        Ok(sp)
    }
}
