// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Compiles uACPI, the interpreter of the firmware's AML (ADR 0200), with
//! Clang, and links it into the crate.
//!
//! For the kernel's target, the C is compiled as the kernel's Rust is:
//! the `kernel` code model, no red zone, no SSE or x87 registers, frame
//! pointers kept, and every function's frame recorded in `.stack_sizes`,
//! which `ob build` holds to the kernel's frame budget (ADR 0058); a frame
//! over it is already an error here. For the build host, it is compiled
//! for the crate's tests, which run uACPI against a simulated machine.
//!
//! Clang is `$RADIX_CLANG`, which `ob` sets to the Clang the ports are
//! built with, or else the newest `clang-N` on `PATH`, or `clang`; the
//! archiver is `$RADIX_AR`, or the newest `llvm-ar-N`, or `llvm-ar`.
//!
//! Only x86-64 has ACPI here, so only its builds compile the C; on other
//! targets the crate's API is there, for the AArch64 stubs' checks, but
//! nothing could link it.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

/// The oldest Clang, as for the ports.
const CLANG_MIN: u32 = 20;

/// The kernel's frame budget, in bytes (ADR 0058).
const FRAME_BUDGET: u32 = 2048;

/// uACPI's options (`vendor/include/uacpi/platform/config.h`): frees say
/// how much they free, which the kernel's heap needs, and zeroed
/// allocations come from the heap, which zeroes them itself.
const DEFINES: &[&str] = &["-DUACPI_SIZED_FREES", "-DUACPI_NATIVE_ALLOC_ZEROED"];

/// The flags of every build.
const COMMON: &[&str] = &[
    "-std=c11",
    "-O2",
    "-g",
    "-fno-strict-aliasing",
    "-fno-omit-frame-pointer",
    "-ffunction-sections",
    "-fdata-sections",
    "-Wall",
    "-Wextra",
    "-Werror",
];

/// The flags of the kernel's build, matching its target specification
/// (`kernel/targets/x86_64-radix-kernel.json`).
const KERNEL: &[&str] = &[
    "--target=x86_64-unknown-none-elf",
    "-ffreestanding",
    "-fno-pic",
    "-fno-pie",
    "-mcmodel=kernel",
    "-mno-red-zone",
    "-mgeneral-regs-only",
    "-mno-omit-leaf-frame-pointer",
    "-fno-stack-protector",
    "-fno-asynchronous-unwind-tables",
    "-fstack-size-section",
];

/// The flags of the host's build, for the tests.
const HOST: &[&str] = &["-fPIC"];

fn main() {
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let out = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor");
    println!("cargo:rerun-if-env-changed=RADIX_CLANG");
    println!("cargo:rerun-if-env-changed=RADIX_AR");
    if arch != "x86_64" {
        return;
    }
    let mut flags: Vec<String> = COMMON.iter().chain(DEFINES).map(ToString::to_string).collect();
    if os == "none" {
        flags.extend(KERNEL.iter().map(ToString::to_string));
        flags.push(format!("-Wframe-larger-than={FRAME_BUDGET}"));
    } else {
        flags.extend(HOST.iter().map(ToString::to_string));
    }
    flags.push(format!("-I{}", dir.join("vendor/include").display()));

    let clang = tool("RADIX_CLANG", "clang");
    let ar = tool("RADIX_AR", "llvm-ar");
    let mut sources: Vec<PathBuf> = std::fs::read_dir(dir.join("vendor/source"))
        .expect("reading vendor/source")
        .map(|entry| entry.expect("reading vendor/source").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "c"))
        .collect();
    sources.sort();
    let objects: Vec<PathBuf> = thread::scope(|scope| {
        let builds: Vec<_> = sources
            .iter()
            .map(|source| scope.spawn(|| compile(&clang, &flags, source, &out)))
            .collect();
        builds.into_iter().map(|build| build.join().expect("a compile thread panicked")).collect()
    });

    let archive = out.join("libuacpi.a");
    let _ = std::fs::remove_file(&archive);
    run(Command::new(&ar).arg("crs").arg(&archive).args(&objects));
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=uacpi");
}

/// Compiles `source` into `out`, returning the object's path.
fn compile(clang: &Path, flags: &[String], source: &Path, out: &Path) -> PathBuf {
    let name = source.file_stem().expect("a source file has a name");
    let object = out.join(name).with_extension("o");
    run(Command::new(clang).args(flags).arg("-c").arg(source).arg("-o").arg(&object));
    object
}

fn run(command: &mut Command) {
    let status = command.status().unwrap_or_else(|err| panic!("cannot run {command:?}: {err}"));
    assert!(status.success(), "{command:?} failed: {status}");
}

/// `$variable`, or the newest `name-N` on `PATH` from [`CLANG_MIN`] up, or
/// `name`.
fn tool(variable: &str, name: &str) -> PathBuf {
    if let Some(path) = env::var_os(variable) {
        return PathBuf::from(path);
    }
    let paths: Vec<PathBuf> =
        env::var_os("PATH").map_or_default(|p| env::split_paths(&p).collect());
    let find = |program: &str| paths.iter().map(|dir| dir.join(program)).find(|p| p.is_file());
    (CLANG_MIN..CLANG_MIN + 20)
        .rev()
        .find_map(|major| find(&format!("{name}-{major}")))
        .or_else(|| find(name))
        .unwrap_or_else(|| {
            panic!("the kernel's AML interpreter needs Clang {CLANG_MIN} or later and LLVM's {name}; run `./ob doctor`")
        })
}
