// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The check of the libc crate's Radix module against mlibc (ADR 0070).
//!
//! Rust's standard library and programs in Rust call the C library
//! through the libc crate, whose Radix module restates mlibc's types,
//! structures, constants and functions in Rust. Where the two disagree,
//! memory is corrupted without a word, so once the rust-std port has
//! built, ob compares them:
//!
//! - every type's and structure's size and alignment, every integer
//!   type's signedness, every public field's offset, and every integer
//!   constant's value are worked out by the C compiler against mlibc's
//!   headers, and then asserted in a crate compiled for Radix against the
//!   new standard library, which fails to compile where Rust disagrees;
//! - every function the module declares must be defined by one of the C
//!   library's archives.
//!
//! The module is read as text: the types, structures and constants at its
//! top level and in its `s!` blocks, and those of the shared `unix`
//! module outside its `cfg_if!` blocks, which Radix shares. Structures
//! and fields under a `cfg` attribute are left out, as is what
//! [`RUST_ONLY`] names.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use crate::ports::{TRIPLE, Toolchain};
use crate::process::{self, step};
use crate::{Ctx, Error, Result, userspace};

/// What the module has that mlibc's headers do not, and why.
const RUST_ONLY: &[(&str, &str)] = &[
    // The libc crate's own name for the type of `ioctl`'s request.
    ("Ioctl", "the libc crate's own"),
    // glibc's name, which the libc crate uses for signal handlers on every
    // Unix; mlibc has no typedef for it.
    ("sighandler_t", "a glibc name the libc crate uses everywhere"),
    // The shared module's ARP flags, which mlibc's <net/if_arp.h> lacks;
    // nothing in the standard library uses them.
    ("ATF_COM", "the shared module's"),
    ("ATF_PERM", "the shared module's"),
    ("ATF_PUBL", "the shared module's"),
    ("ATF_USETRAILERS", "the shared module's"),
];

/// The C types of the records whose names [`c_record`] would get wrong.
const RECORD_NAMES: &[(&str, &str)] = &[
    // A typedef of an untagged structure.
    ("fd_set", "fd_set"),
    // A union, which the libc crate has as a structure.
    ("sigval", "union sigval"),
];

/// The names C knows a field by, where Rust's differ: Rust has the
/// seconds and nanoseconds of a `struct timespec` as two fields, and
/// glibc's names for the terminal speeds.
const FIELD_NAMES: &[(&str, &str, &str)] = &[
    ("stat", "st_atime_nsec", "st_atim.tv_nsec"),
    ("stat", "st_mtime_nsec", "st_mtim.tv_nsec"),
    ("stat", "st_ctime_nsec", "st_ctim.tv_nsec"),
    ("termios", "c_ispeed", "c_ibaud"),
    ("termios", "c_ospeed", "c_obaud"),
];

/// The headers the C side includes: those of every part of the C library
/// the libc crate covers.
const HEADERS: &[&str] = &[
    "arpa/inet.h",
    "dirent.h",
    "dlfcn.h",
    "elf.h",
    "errno.h",
    "fcntl.h",
    "fnmatch.h",
    "getopt.h",
    "glob.h",
    "grp.h",
    "langinfo.h",
    "limits.h",
    "link.h",
    "locale.h",
    "net/if.h",
    "net/if_arp.h",
    "netdb.h",
    "netinet/in.h",
    "netinet/tcp.h",
    "poll.h",
    "pthread.h",
    "pty.h",
    "pwd.h",
    "regex.h",
    "sched.h",
    "semaphore.h",
    "signal.h",
    "spawn.h",
    "stdio.h",
    "stdlib.h",
    "string.h",
    "sys/auxv.h",
    "sys/file.h",
    "sys/ipc.h",
    "sys/ioctl.h",
    "sys/mman.h",
    "sys/resource.h",
    "sys/select.h",
    "sys/socket.h",
    "sys/stat.h",
    "sys/statvfs.h",
    "sys/time.h",
    "sys/times.h",
    "sys/types.h",
    "sys/uio.h",
    "sys/un.h",
    "sys/utsname.h",
    "sys/wait.h",
    "syslog.h",
    "termios.h",
    "time.h",
    "unistd.h",
    "utime.h",
    "utmpx.h",
    "wordexp.h",
];

/// Rust's integer types, as the libc crate names them.
const INTEGERS: &[&str] = &[
    "i8",
    "i16",
    "i32",
    "i64",
    "isize",
    "u8",
    "u16",
    "u32",
    "u64",
    "usize",
    "c_char",
    "c_schar",
    "c_uchar",
    "c_short",
    "c_ushort",
    "c_int",
    "c_uint",
    "c_long",
    "c_ulong",
    "c_longlong",
    "c_ulonglong",
];

/// A structure or union the module defines.
#[derive(Debug, Default)]
struct Record {
    name: String,
    union: bool,
    /// Its public fields that every build has.
    fields: Vec<String>,
}

/// What the module declares.
#[derive(Debug, Default)]
struct Module {
    /// Type aliases: name and type.
    types: Vec<(String, String)>,
    records: Vec<Record>,
    /// Constants: name and type.
    constants: Vec<(String, String)>,
    /// Functions, by the symbol they link to.
    functions: Vec<String>,
}

/// One value the C compiler works out, and what Rust must agree with.
struct Item {
    /// The C expression.
    c: String,
    /// What Rust says, as an expression of type `u64`, and how many of
    /// its low bytes count.
    rust: String,
    bytes: Option<String>,
    /// What it is, for the report.
    what: String,
    /// The name the C side needs, which [`RUST_ONLY`] may excuse.
    name: String,
}

/// Compares the libc crate of the Rust standard library the rust-std
/// port installed in `toolchain` with mlibc's headers and archives.
pub fn check(ctx: &Ctx, toolchain: &Toolchain) -> Result<()> {
    step("Checking the libc crate's Radix module against the C library");
    let libc = toolchain.dir.join("rust/vendor/libc/src");
    let mut module = Module::default();
    parse(&read(&libc.join("unix/mod.rs"))?, &mut module);
    parse(&read(&libc.join("unix/radix/mod.rs"))?, &mut module);
    let work = ctx.target.join("libc-check");
    process::create_dir(&work)?;
    let items = items(&module);
    let values = c_values(ctx, toolchain, &work, &items)?;
    let mut problems = rust_disagrees(ctx, toolchain, &work, &items, &values)?;
    problems.extend(missing_functions(ctx, toolchain, &module)?);
    if problems.is_empty() {
        step(&format!(
            "The libc crate agrees with the C library on {} sizes, alignments, offsets and \
             constants and {} functions",
            items.len(),
            module.functions.len()
        ));
        return Ok(());
    }
    Err(Error::new(format!(
        "the libc crate's Radix module disagrees with the C library:\n  {}",
        problems.join("\n  ")
    ))
    .hint("fix the module in ports/rust-std/patches/0004-libc-radix.patch"))
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path)
        .map_err(|err| Error::new(format!("reading {}: {err}", path.display())))
}

/// Reads the declarations of a module of the libc crate into `module`:
/// what its top level and its top-level `s!` and `extern` blocks declare.
fn parse(text: &str, module: &mut Module) {
    let mut block = None::<&str>;
    let mut record: Option<Record> = None;
    // Whether an attribute leaves the next item out of some builds.
    let mut conditional = false;
    let mut link_name = None::<String>;
    for raw in text.lines() {
        let line = raw.split("//").next().unwrap_or("").trim_end();
        let indent = line.len() - line.trim_start().len();
        let line = line.trim_start();
        if line.is_empty() {
            continue;
        }
        if indent == 0 {
            // A top-level item; a block's closing brace ends it.
            block = None;
            if line.starts_with("s! {") || line.starts_with("s_no_extra_traits! {") {
                block = Some("records");
            } else if line.starts_with("extern \"C\" {") {
                block = Some("functions");
            } else if let Some((name, ty)) = line
                .strip_prefix("pub type ")
                .and_then(|rest| rest.trim_end_matches(';').split_once(" = "))
            {
                module.types.push((name.to_string(), ty.to_string()));
            } else if let Some(rest) = line.strip_prefix("pub const ")
                && let Some((name, rest)) = rest.split_once(": ")
                && let Some((ty, _)) = rest.split_once(" = ")
            {
                module.constants.push((name.to_string(), ty.to_string()));
            }
            continue;
        }
        if line.starts_with("#[") {
            conditional |= line.starts_with("#[cfg(");
            if let Some(rest) = line.strip_prefix("#[link_name = \"") {
                link_name = rest.split('"').next().map(str::to_string);
            }
            continue;
        }
        let skip = std::mem::take(&mut conditional);
        match (block, indent) {
            (Some("records"), 4) => {
                if line == "}" {
                    module.records.extend(record.take());
                } else if let Some(rest) =
                    line.strip_prefix("pub struct ").or_else(|| line.strip_prefix("pub union "))
                {
                    let name = rest.trim_end_matches('{').trim().to_string();
                    let union = line.starts_with("pub union");
                    record = (!skip).then(|| Record { name, union, fields: Vec::new() });
                }
            }
            (Some("records"), 8) => {
                if let Some(record) = record.as_mut()
                    && let Some(rest) = line.strip_prefix("pub ")
                {
                    let name = rest.split(':').next().unwrap_or("").trim();
                    if !skip && !name.starts_with('_') {
                        record.fields.push(name.to_string());
                    }
                }
            }
            (Some("functions"), 4) => {
                let link_name = link_name.take();
                if let Some(rest) = line.strip_prefix("pub fn ")
                    && !skip
                {
                    let name = rest.split('(').next().unwrap_or("").trim().to_string();
                    module.functions.push(link_name.unwrap_or(name));
                }
            }
            _ => {}
        }
    }
}

/// The type `ty` names, through the module's aliases: an integer type of
/// [`INTEGERS`], a pointer (`*`), or something else.
fn resolve<'a>(module: &'a Module, mut ty: &'a str) -> &'a str {
    for _ in 0..8 {
        ty = ty.trim_start_matches("crate::");
        if ty.starts_with('*') || INTEGERS.contains(&ty) {
            return ty;
        }
        match module.types.iter().rev().find(|(name, _)| name == ty) {
            Some((_, next)) => ty = next,
            None => return ty,
        }
    }
    ty
}

/// The C type of record `name`: a typedef for the `_t` names and the
/// capitalised ones, a structure's or union's tag for the others.
fn c_record(name: &str, union: bool) -> String {
    if let Some((_, c)) = RECORD_NAMES.iter().find(|(rust, _)| *rust == name) {
        c.to_string()
    } else if name.ends_with("_t") || name.starts_with(|c: char| c.is_ascii_uppercase()) {
        name.to_string()
    } else if union {
        format!("union {name}")
    } else {
        format!("struct {name}")
    }
}

/// What to compare.
fn items(module: &Module) -> Vec<Item> {
    let mut items = Vec::new();
    let mut add = |c: String, rust: String, bytes: Option<String>, what: String, name: &str| {
        items.push(Item { c, rust, bytes, what, name: name.to_string() });
    };
    for (name, ty) in &module.types {
        let resolved = resolve(module, ty);
        let c = name;
        add(
            format!("sizeof({c})"),
            format!("size_of::<libc::{name}>()"),
            None,
            format!("size of {name}"),
            name,
        );
        if INTEGERS.contains(&resolved) {
            add(
                format!("({c})-1 < ({c})0"),
                format!("(<libc::{name}>::MIN != 0) as u64"),
                None,
                format!("signedness of {name}"),
                name,
            );
        }
    }
    for record in &module.records {
        let name = &record.name;
        let c = c_record(name, record.union);
        add(
            format!("sizeof({c})"),
            format!("size_of::<libc::{name}>()"),
            None,
            format!("size of {name}"),
            name,
        );
        add(
            format!("_Alignof({c})"),
            format!("align_of::<libc::{name}>()"),
            None,
            format!("alignment of {name}"),
            name,
        );
        for field in &record.fields {
            let c_field = FIELD_NAMES
                .iter()
                .find(|(r, f, _)| r == name && f == field)
                .map_or(field.as_str(), |(_, _, c)| c);
            add(
                format!("__builtin_offsetof({c}, {c_field})"),
                format!("offset_of!(libc::{name}, {field})"),
                None,
                format!("offset of {name}.{field}"),
                name,
            );
        }
    }
    for (name, ty) in &module.constants {
        if INTEGERS.contains(&resolve(module, ty)) {
            add(
                name.clone(),
                format!("libc::{name}"),
                Some(format!("size_of_val(&libc::{name})")),
                format!("value of {name}"),
                name,
            );
        }
    }
    items.retain(|item| !RUST_ONLY.iter().any(|(name, _)| *name == item.name));
    items
}

/// Works out every item's C expression with the C compiler, against
/// mlibc's headers.
fn c_values(ctx: &Ctx, toolchain: &Toolchain, work: &Path, items: &[Item]) -> Result<Vec<i64>> {
    let mut source = String::from("#define _GNU_SOURCE\n#include <stddef.h>\n");
    for header in HEADERS {
        let _ = writeln!(source, "#include <{header}>");
    }
    let first = source.lines().count() + 1;
    for (i, item) in items.iter().enumerate() {
        let _ = writeln!(source, "const long long v_{i} = (long long)({});", item.c);
    }
    let c = work.join("values.c");
    let ir = work.join("values.ll");
    std::fs::write(&c, source)
        .map_err(|err| Error::new(format!("writing {}: {err}", c.display())))?;
    let mut cmd = Command::new(toolchain.cc());
    cmd.args(["-S", "-emit-llvm", "-O0", "-w", "-ferror-limit=0", "-o"]).arg(&ir).arg(&c);
    let output =
        cmd.output().map_err(|err| Error::new(format!("running the C compiler: {err}")))?;
    if !output.status.success() {
        // What C does not know is what the module has and mlibc lacks.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut unknown: Vec<String> = stderr
            .lines()
            .filter_map(|line| {
                let rest = line.split("values.c:").nth(1)?;
                let number: usize = rest.split(':').next()?.parse().ok()?;
                rest.contains(": error:").then(|| number.checked_sub(first)).flatten()
            })
            .filter_map(|index| {
                items.get(index).map(|item| format!("{}: not in mlibc's headers", item.what))
            })
            .collect();
        unknown.dedup();
        if unknown.is_empty() {
            return Err(Error::new(format!("the C compiler failed:\n{}", stderr.trim_end())));
        }
        return Err(Error::new(format!(
            "the libc crate's Radix module disagrees with the C library:\n  {}",
            unknown.join("\n  ")
        ))
        .hint("fix the module in ports/rust-std/patches/0004-libc-radix.patch"));
    }
    let _ = ctx;
    let text = read(&ir)?;
    let mut values = vec![None; items.len()];
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("@v_") else { continue };
        let Some((index, rest)) = rest.split_once(' ') else { continue };
        let value = rest.split("constant i64 ").nth(1).and_then(|v| v.split(',').next());
        if let (Ok(index), Some(Ok(value))) = (index.parse::<usize>(), value.map(str::parse::<i64>))
            && index < values.len()
        {
            values[index] = Some(value);
        }
    }
    values
        .into_iter()
        .enumerate()
        .map(|(i, value)| {
            value.ok_or_else(|| {
                Error::new(format!("no value for {} in {}", items[i].what, ir.display()))
            })
        })
        .collect()
}

/// Asserts every item's value in a crate compiled for Radix against the
/// standard library's libc crate, and returns what Rust disagrees with.
fn rust_disagrees(
    ctx: &Ctx,
    toolchain: &Toolchain,
    work: &Path,
    items: &[Item],
    values: &[i64],
) -> Result<Vec<String>> {
    let mut source = String::from(
        "#![feature(rustc_private)]\n#![no_std]\nextern crate libc;\n\
         use core::mem::{align_of, offset_of, size_of, size_of_val};\n\
         const fn bits(value: u64, bytes: usize) -> u64 {\n\
         \x20   if bytes >= 8 { value } else { value & ((1 << (bytes * 8)) - 1) }\n}\n",
    );
    for (item, value) in items.iter().zip(values) {
        let bytes = item.bytes.as_deref().unwrap_or("8");
        let _ = writeln!(
            source,
            "const _: () = assert!(bits(({}) as u64, {bytes}) == bits({value}_i64 as u64, {bytes}), \"{}: the C library's is {value}\");",
            item.rust, item.what
        );
    }
    let rs = work.join("check.rs");
    std::fs::write(&rs, source)
        .map_err(|err| Error::new(format!("writing {}: {err}", rs.display())))?;
    let mut cmd = Command::new("rustc");
    cmd.current_dir(&ctx.root)
        .args([
            "--edition",
            "2024",
            "--crate-type",
            "lib",
            "--emit",
            "metadata",
            "-Zunstable-options",
        ])
        .arg("--target")
        .arg(ctx.root.join(userspace::STD_TARGET_SPEC))
        .arg("--sysroot")
        .arg(toolchain.dir.join("rust"))
        .arg("-o")
        .arg(work.join("check.rmeta"))
        .arg(&rs);
    for var in ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"] {
        cmd.env_remove(var);
    }
    let output = cmd.output().map_err(|err| Error::new(format!("running rustc: {err}")))?;
    if output.status.success() {
        return Ok(Vec::new());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let found: Vec<String> = stderr
        .lines()
        .filter_map(|line| line.split("evaluation panicked: ").nth(1))
        .map(|message| format!("Rust's {message}"))
        .collect();
    if found.is_empty() {
        return Err(Error::new(format!("compiling the check failed:\n{}", stderr.trim_end())));
    }
    Ok(found)
}

/// The functions the module declares that none of the C library's
/// archives defines.
fn missing_functions(ctx: &Ctx, toolchain: &Toolchain, module: &Module) -> Result<Vec<String>> {
    let lib = ctx.out.join("sysroot/system/lib");
    let mut archives: Vec<_> = std::fs::read_dir(&lib)
        .map_err(|err| Error::new(format!("reading {}: {err}", lib.display())))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|e| e == "a"))
        .collect();
    archives.sort();
    let mut cmd = Command::new(toolchain.bin().join(format!("{TRIPLE}-nm")));
    cmd.args(["--defined-only", "--format=just-symbols"]).args(&archives);
    let output = process::output(ctx, &mut cmd)?;
    let defined: std::collections::BTreeSet<&str> =
        std::str::from_utf8(&output.stdout).unwrap_or("").lines().collect();
    Ok(module
        .functions
        .iter()
        .filter(|name| !defined.contains(name.as_str()))
        .map(|name| format!("function {name}: the C library does not define it"))
        .collect())
}
