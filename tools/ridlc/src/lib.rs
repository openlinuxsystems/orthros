// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `ridlc`, the Radix interface definition compiler.
//!
//! The kernel's system calls and object interfaces are written down once,
//! in `abi/*.ridl`, and everything that must agree on them is generated
//! from there: the Rust bindings the kernel and user programs share
//! (`kernel/abi`, built from its build script), the C headers for the C
//! library, and the man(2) pages. The language is described in
//! `docs/design/ipc.md` and ADR 0033; in short, a file is a module of
//! documented items:
//!
//! ```text
//! //! What the module is about.
//! module memory;
//!
//! /// A constant.
//! const PAGE_SIZE: u64 = 4096;
//! /// An enumeration.
//! enum Opcode: u8 { /// Doc. nop = 0; }
//! /// A set of flags (bit numbers).
//! flags Rights: u64 { /// Doc. read = 0; }
//! /// Errors, with a domain number and an errno each.
//! errors kernel = 1 { /// Doc. empty_slot = 3 => EBADF; }
//! /// A structure with a fixed layout, shared through memory.
//! struct Cqe { /// Doc. user_data: u64; }
//! /// A system call: arguments and results in registers.
//! syscall yield = 6 () -> ();
//! /// An interface: methods called on a capability with cap_call.
//! interface memory = 0x0001 version 1 {
//!     kind = memory;
//!     /// Doc.
//!     method info = 2 () -> (pages: u64) raises [empty_slot];
//! }
//! ```
//!
//! Every item, member and parameter list needs a `///` comment: the
//! comments become the documentation of the Rust items, the C
//! declarations and the man pages, so an undocumented interface is an
//! error, not a warning.

use std::fmt;
use std::path::Path;

mod check;
mod gen_c;
mod gen_man;
mod gen_rust;
mod lex;
mod parse;

pub use gen_c::c_headers;
pub use gen_man::man_pages;
pub use gen_rust::rust;

/// A primitive type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prim {
    /// `u8`.
    U8,
    /// `u16`.
    U16,
    /// `u32`.
    U32,
    /// `u64`.
    U64,
    /// `i32`.
    I32,
    /// `i64`.
    I64,
    /// `bool`, 0 or 1.
    Bool,
}

impl Prim {
    /// The type's size in bytes.
    pub fn size(self) -> usize {
        match self {
            Prim::U8 | Prim::Bool => 1,
            Prim::U16 => 2,
            Prim::U32 | Prim::I32 => 4,
            Prim::U64 | Prim::I64 => 8,
        }
    }

    /// Its name in the language and in Rust.
    pub fn name(self) -> &'static str {
        match self {
            Prim::U8 => "u8",
            Prim::U16 => "u16",
            Prim::U32 => "u32",
            Prim::U64 => "u64",
            Prim::I32 => "i32",
            Prim::I64 => "i64",
            Prim::Bool => "bool",
        }
    }

    fn parse(name: &str) -> Option<Prim> {
        Some(match name {
            "u8" => Prim::U8,
            "u16" => Prim::U16,
            "u32" => Prim::U32,
            "u64" => Prim::U64,
            "i32" => Prim::I32,
            "i64" => Prim::I64,
            "bool" => Prim::Bool,
            _ => return None,
        })
    }
}

/// Which way a buffer's bytes go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// The kernel reads the buffer.
    In,
    /// The kernel writes the buffer.
    Out,
}

/// A type in a parameter or field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Type {
    /// A primitive.
    Prim(Prim),
    /// A capability slot number (32 bits).
    Slot,
    /// A user address (64 bits).
    Addr,
    /// An enumeration, set of flags or structure, by name.
    Named(String),
    /// A fixed-size array.
    Array(Box<Type>, usize),
    /// A user buffer: its address and length, two words.
    Bytes(Dir),
}

/// A documented name with a number: an enumeration variant, a flag (its
/// bit number) or an error (its code).
#[derive(Clone, Debug)]
pub struct Member {
    /// Documentation lines.
    pub doc: Vec<String>,
    /// Name, in snake case.
    pub name: String,
    /// Value, bit number or code.
    pub value: u64,
    /// For errors: the errno it maps to.
    pub errno: Option<String>,
    /// Line in the file.
    pub line: usize,
}

/// A field of a structure, or a parameter.
#[derive(Clone, Debug)]
pub struct Field {
    /// Documentation lines (structure fields only).
    pub doc: Vec<String>,
    /// Name, in snake case.
    pub name: String,
    /// Type.
    pub ty: Type,
    /// Line in the file.
    pub line: usize,
}

/// A system call or method.
#[derive(Clone, Debug)]
pub struct Call {
    /// Documentation lines.
    pub doc: Vec<String>,
    /// Name, in snake case.
    pub name: String,
    /// System call number, or method number within its interface.
    pub number: u64,
    /// Arguments.
    pub args: Vec<Field>,
    /// Results.
    pub results: Vec<Field>,
    /// Errors it may return, by name.
    pub raises: Vec<String>,
    /// Line in the file.
    pub line: usize,
}

/// What an item is.
#[derive(Clone, Debug)]
pub enum ItemKind {
    /// `const NAME: type = value;`
    Const {
        /// Type (a primitive).
        ty: Prim,
        /// Value.
        value: u64,
    },
    /// `enum Name: repr { variants }`
    Enum {
        /// Representation.
        repr: Prim,
        /// Variants.
        variants: Vec<Member>,
    },
    /// `flags Name: repr { bits }`
    Flags {
        /// Representation.
        repr: Prim,
        /// Flags, with their bit numbers.
        bits: Vec<Member>,
    },
    /// `errors name = domain { errors }`
    Errors {
        /// Domain number.
        domain: u64,
        /// Errors, with their codes.
        errors: Vec<Member>,
    },
    /// `struct Name { fields }`
    Struct {
        /// Fields, in layout order.
        fields: Vec<Field>,
    },
    /// `syscall name = number (args) -> (results);`
    Syscall(Call),
    /// `interface name = id version v { kind = k; methods }`
    Interface {
        /// Interface number, the high half of its methods' labels.
        id: u64,
        /// Version.
        version: u64,
        /// The object kind that answers it.
        kind: Option<String>,
        /// Methods.
        methods: Vec<Call>,
    },
}

/// A documented item of a module.
#[derive(Clone, Debug)]
pub struct Item {
    /// Documentation lines.
    pub doc: Vec<String>,
    /// Name.
    pub name: String,
    /// What it is.
    pub kind: ItemKind,
    /// Line in the file.
    pub line: usize,
}

/// One `.ridl` file.
#[derive(Clone, Debug)]
pub struct Module {
    /// The file, for messages.
    pub path: String,
    /// The module's name.
    pub name: String,
    /// Its `//!` documentation.
    pub doc: Vec<String>,
    /// Its items.
    pub items: Vec<Item>,
}

/// Every module of an ABI, checked.
#[derive(Clone, Debug)]
pub struct Abi {
    /// The modules, sorted by name.
    pub modules: Vec<Module>,
}

impl Abi {
    /// The item named `name` (a type) and the module that defines it.
    pub fn find(&self, name: &str) -> Option<(&Module, &Item)> {
        self.modules.iter().find_map(|module| {
            module
                .items
                .iter()
                .find(|item| item.name == name && !matches!(item.kind, ItemKind::Const { .. }))
                .map(|item| (module, item))
        })
    }

    /// The system calls, by number.
    pub fn syscalls(&self) -> Vec<(&Module, &Call)> {
        let mut calls: Vec<_> = self
            .modules
            .iter()
            .flat_map(|module| {
                module.items.iter().filter_map(move |item| match &item.kind {
                    ItemKind::Syscall(call) => Some((module, call)),
                    _ => None,
                })
            })
            .collect();
        calls.sort_by_key(|(_, call)| call.number);
        calls
    }

    /// The error sets.
    pub fn error_sets(&self) -> Vec<(&Module, &Item)> {
        self.modules
            .iter()
            .flat_map(|module| {
                module
                    .items
                    .iter()
                    .filter(|item| matches!(item.kind, ItemKind::Errors { .. }))
                    .map(move |item| (module, item))
            })
            .collect()
    }

    /// The error named `name`, from any set.
    pub fn error(&self, name: &str) -> Option<&Member> {
        self.error_sets().into_iter().find_map(|(_, item)| match &item.kind {
            ItemKind::Errors { errors, .. } => errors.iter().find(|error| error.name == name),
            _ => None,
        })
    }

    /// How many machine words a type takes as a parameter.
    pub fn words(&self, ty: &Type) -> usize {
        match ty {
            Type::Array(inner, len) => self.words(inner) * len,
            Type::Bytes(_) => 2,
            _ => 1,
        }
    }

    /// The size and alignment of a type in a structure.
    pub fn layout(&self, ty: &Type) -> Option<(usize, usize)> {
        match ty {
            Type::Prim(prim) => Some((prim.size(), prim.size())),
            Type::Slot => Some((4, 4)),
            Type::Addr => Some((8, 8)),
            Type::Array(inner, len) => {
                let (size, align) = self.layout(inner)?;
                Some((size * len, align))
            }
            Type::Named(name) => match &self.find(name)?.1.kind {
                ItemKind::Struct { fields } => {
                    let mut offset: usize = 0;
                    let mut align: usize = 1;
                    for field in fields {
                        let (size, field_align) = self.layout(&field.ty)?;
                        offset = offset.next_multiple_of(field_align) + size;
                        align = align.max(field_align);
                    }
                    Some((offset.next_multiple_of(align), align))
                }
                _ => None,
            },
            Type::Bytes(_) => None,
        }
    }
}

/// A problem in a `.ridl` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// The file.
    pub path: String,
    /// The line, from 1 (0 when it is about the whole file).
    pub line: usize,
    /// What is wrong.
    pub message: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(f, "{}: {}", self.path, self.message)
        } else {
            write!(f, "{}:{}: {}", self.path, self.line, self.message)
        }
    }
}

/// Parses and checks a set of files, given as (path, contents).
pub fn compile(files: &[(String, String)]) -> Result<Abi, Vec<Error>> {
    let mut modules = Vec::new();
    let mut errors = Vec::new();
    for (path, source) in files {
        match parse::module(path, source) {
            Ok(module) => modules.push(module),
            Err(error) => errors.push(error),
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    modules.sort_by(|a, b| a.name.cmp(&b.name));
    let abi = Abi { modules };
    let problems = check::check(&abi);
    if problems.is_empty() { Ok(abi) } else { Err(problems) }
}

/// Reads and compiles every `.ridl` file in `dir`.
pub fn compile_dir(dir: &Path) -> Result<Abi, Vec<Error>> {
    let read_error =
        |message: String| vec![Error { path: dir.display().to_string(), line: 0, message }];
    let entries = std::fs::read_dir(dir).map_err(|err| read_error(err.to_string()))?;
    let mut files = Vec::new();
    for entry in entries {
        let path = entry.map_err(|err| read_error(err.to_string()))?.path();
        if path.extension().is_some_and(|ext| ext == "ridl") {
            let source = std::fs::read_to_string(&path).map_err(|err| {
                vec![Error { path: path.display().to_string(), line: 0, message: err.to_string() }]
            })?;
            files.push((path.display().to_string(), source));
        }
    }
    files.sort();
    if files.is_empty() {
        return Err(read_error("no .ridl files".into()));
    }
    compile(&files)
}

/// The POSIX errno values ridl files may name, with their numbers on
/// Linux, which the C library's numbering follows.
pub const ERRNOS: &[(&str, i32)] = &[
    ("EPERM", 1),
    ("ENOENT", 2),
    ("ESRCH", 3),
    ("EINTR", 4),
    ("EIO", 5),
    ("ENXIO", 6),
    ("E2BIG", 7),
    ("ENOEXEC", 8),
    ("EBADF", 9),
    ("ECHILD", 10),
    ("EAGAIN", 11),
    ("ENOMEM", 12),
    ("EACCES", 13),
    ("EFAULT", 14),
    ("ENOTBLK", 15),
    ("EBUSY", 16),
    ("EEXIST", 17),
    ("EXDEV", 18),
    ("ENODEV", 19),
    ("ENOTDIR", 20),
    ("EISDIR", 21),
    ("EINVAL", 22),
    ("ENOTTY", 25),
    ("EFBIG", 27),
    ("ENOSPC", 28),
    ("ESPIPE", 29),
    ("EROFS", 30),
    ("EMLINK", 31),
    ("EPIPE", 32),
    ("ERANGE", 34),
    ("EDEADLK", 35),
    ("ENAMETOOLONG", 36),
    ("ENOSYS", 38),
    ("ENOTEMPTY", 39),
    ("ELOOP", 40),
    ("ENOTSOCK", 88),
    ("EMSGSIZE", 90),
    ("EOPNOTSUPP", 95),
    ("EISCONN", 106),
    ("ENOTCONN", 107),
    ("ETOOMANYREFS", 109),
    ("ETIMEDOUT", 110),
    ("ECONNREFUSED", 111),
    ("EUCLEAN", 117),
];

/// The number of an errno name.
pub fn errno(name: &str) -> Option<i32> {
    ERRNOS.iter().find(|(n, _)| *n == name).map(|(_, number)| *number)
}

/// `snake_case` as `CamelCase`.
pub fn camel(name: &str) -> String {
    name.split('_')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// `CamelCase` as `snake_case`.
pub fn snake(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The first sentence of a documentation comment.
pub fn summary(doc: &[String]) -> String {
    let text = doc.join(" ");
    let text = text.trim();
    match text.find(". ") {
        Some(end) => text[..end].to_string(),
        None => text.trim_end_matches('.').to_string(),
    }
}

#[cfg(test)]
mod tests;
