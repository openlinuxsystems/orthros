// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;

fn abi_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../abi")
}

fn one(source: &str) -> Result<Abi, Vec<Error>> {
    compile(&[("test.ridl".to_string(), source.to_string())])
}

fn messages(source: &str) -> Vec<String> {
    one(source).err().unwrap_or_default().into_iter().map(|e| e.message).collect()
}

const ERRORS: &str = "
/// Errors.
errors kernel = 1 {
    /// Bad.
    invalid_argument = 8 => EINVAL;
    /// Gone.
    gone = 14 => EPIPE;
}
/// Calls.
syscall cap_call = 0 (cap: slot, info: u64, words: [u64; 8]) -> (badge: u64, info: u64, words: [u64; 8]);
";

fn with_module(body: &str) -> String {
    format!("//! A test.\nmodule test;\n{ERRORS}\n{body}")
}

#[test]
fn the_repository_abi_compiles() {
    let abi = compile_dir(&abi_dir()).unwrap_or_else(|errors| {
        panic!("{}", errors.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n"))
    });
    let syscalls = abi.syscalls();
    assert_eq!(syscalls.len(), 12, "Radix has twelve system calls");
    for (number, (_, call)) in syscalls.iter().enumerate() {
        assert_eq!(call.number, number as u64, "system calls are numbered from 0 without gaps");
    }
    let names: Vec<&str> = syscalls.iter().map(|(_, call)| call.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "cap_call",
            "send",
            "recv",
            "reply_recv",
            "notify",
            "wait",
            "yield",
            "map",
            "unmap",
            "ring_setup",
            "ring_enter",
            "debug_log"
        ]
    );
}

#[test]
fn every_item_needs_documentation() {
    let problems = messages(&with_module("const UNDOCUMENTED: u64 = 1;"));
    assert!(
        problems.iter().any(|m| m.contains("constant `UNDOCUMENTED` has no documentation")),
        "{problems:?}"
    );
    let problems = messages(&with_module("/// Doc.\nenum E: u8 { a = 0; }"));
    assert!(
        problems.iter().any(|m| m.contains("variant `a` has no documentation")),
        "{problems:?}"
    );
    let problems = messages("module test;\n");
    assert!(
        problems.iter().any(|m| m.contains("module `test` has no documentation")),
        "{problems:?}"
    );
}

#[test]
fn numbers_and_names_must_be_unique() {
    let problems = messages(&with_module(
        "/// A.\nsyscall a = 0 () -> ();\n/// B.\nenum E: u8 { /// X.\n x = 1; /// Y.\n x = 1; }",
    ));
    assert!(problems.iter().any(|m| m.contains("system call number 0 is taken")), "{problems:?}");
    assert!(problems.iter().any(|m| m.contains("variant `x` is defined twice")), "{problems:?}");
    assert!(problems.iter().any(|m| m.contains("variant value 1 is used twice")), "{problems:?}");
}

#[test]
fn calls_fit_their_registers() {
    let problems = messages(&with_module(
        "/// Too many.\nsyscall big = 1 (a: [u64; 8], b: u64, c: u64, d: u64) -> ();",
    ));
    assert!(problems.iter().any(|m| m.contains("take 11 words; at most 10 fit")), "{problems:?}");
    let problems = messages(&with_module(
        "/// I.\ninterface i = 1 version 1 {\n /// M.\n method m = 1 (a: [u64; 8], b: in bytes) -> ();\n}",
    ));
    assert!(problems.iter().any(|m| m.contains("take 10 words; at most 8 fit")), "{problems:?}");
    let problems = messages(&with_module("/// R.\nsyscall r = 1 () -> () raises [nope];"));
    assert!(problems.iter().any(|m| m.contains("unknown error `nope`")), "{problems:?}");
}

#[test]
fn structures_have_no_hidden_padding() {
    let problems = messages(&with_module("/// S.\nstruct S { /// A.\n a: u8; /// B.\n b: u64; }"));
    assert!(problems.iter().any(|m| m.contains("would be padded to offset 8")), "{problems:?}");
    let problems = messages(&with_module("/// S.\nstruct S { /// A.\n a: u64; /// B.\n b: u8; }"));
    assert!(problems.iter().any(|m| m.contains("padded at its end from 9 to 16")), "{problems:?}");
    let abi = one(&with_module(
        "/// S.\nstruct S { /// A.\n a: u32; /// B.\n b: [u8; 4]; /// C.\n c: u64; }",
    ))
    .expect("a padded-out structure is fine");
    assert_eq!(abi.layout(&Type::Named("S".into())), Some((16, 8)));
}

#[test]
fn syntax_errors_name_the_line() {
    let error = one("//! Doc.\nmodule test;\n\n/// Doc.\nconst X: u64 = ;").unwrap_err();
    assert_eq!(error[0].line, 5);
    assert!(error[0].message.contains("expected a number"), "{error:?}");
    let error = one("//! Doc.\nmodule test;\n/// Doc.\nconst X: u64 = 0x12g;").unwrap_err();
    assert!(error[0].message.contains("is not a number"), "{error:?}");
}

#[test]
fn summaries_take_the_first_sentence() {
    let doc = vec!["Waits for a word. Then".to_string(), "returns it.".to_string()];
    assert_eq!(summary(&doc), "Waits for a word");
    assert_eq!(camel("cap_call"), "CapCall");
    assert_eq!(snake("ObjectKind"), "object_kind");
}

/// A scratch directory under the build's target directory.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/ridlc-test").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("cannot create a scratch directory");
    dir
}

#[test]
fn the_rust_bindings_compile_without_std() {
    let abi = compile_dir(&abi_dir()).expect("the ABI compiles");
    let dir = scratch("rust");
    let source = format!(
        "#![no_std]\n#![deny(warnings)]\n#![allow(missing_docs)]\n{}\n\
         pub fn check() -> bool {{\n    \
         let args = memory::RetypeArgs {{ kind: capability::ObjectKind::Frame, order: 2, to: 9, count: 3 }};\n    \
         memory::RetypeArgs::decode(&args.encode()) == Ok(args)\n        \
         && memory::RetypeArgs::decode(&[99, 0, 0, 0, 0, 0, 0, 0]) == Err(Invalid(0))\n        \
         && errors::Kernel::from_status(errors::Kernel::Gone.status(7)) == Some(errors::Kernel::Gone)\n\
         }}\n",
        rust(&abi)
    );
    let file = dir.join("abi.rs");
    std::fs::write(&file, source).expect("cannot write the bindings");
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let output = Command::new(rustc)
        .args(["--edition", "2024", "--crate-type", "lib", "--crate-name", "abi", "-o"])
        .arg(dir.join("libabi.rlib"))
        .arg(&file)
        .output()
        .expect("cannot run rustc");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn the_c_headers_compile() {
    let Some(cc) = ["cc", "gcc", "clang"]
        .into_iter()
        .find(|cc| Command::new(cc).arg("--version").output().is_ok())
    else {
        eprintln!("no C compiler; skipping");
        return;
    };
    let abi = compile_dir(&abi_dir()).expect("the ABI compiles");
    let dir = scratch("c");
    for (name, text) in c_headers(&abi) {
        let path = dir.join(&name);
        std::fs::create_dir_all(path.parent().expect("a header has a parent")).expect("mkdir");
        std::fs::write(path, text).expect("cannot write a header");
    }
    let test = dir.join("test.c");
    std::fs::write(
        &test,
        "#include <radix/radix.h>\n\
         int f(void) { return radix_kernel_errno(UINT64_C(0x10003)) + (int)sizeof(struct radix_sqe); }\n",
    )
    .expect("cannot write the test");
    let output = Command::new(cc)
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-pedantic", "-fsyntax-only", "-I"])
        .arg(&dir)
        .arg(&test)
        .output()
        .expect("cannot run the C compiler");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn the_man_pages_render() {
    let abi = compile_dir(&abi_dir()).expect("the ABI compiles");
    let pages = man_pages(&abi);
    for (_, call) in abi.syscalls() {
        assert!(pages.iter().any(|(name, _)| *name == format!("radix_{}.2.scd", call.name)));
    }
    assert!(pages.iter().any(|(name, _)| name == "radix-errors.2.scd"));
    if Command::new("scdoc").arg("-h").output().is_err() {
        eprintln!("no scdoc; skipping the rendering");
        return;
    }
    for (name, text) in pages {
        let mut child = Command::new("scdoc")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("cannot run scdoc");
        use std::io::Write;
        child.stdin.take().expect("stdin").write_all(text.as_bytes()).expect("write");
        let output = child.wait_with_output().expect("scdoc");
        assert!(output.status.success(), "{name}: {}", String::from_utf8_lossy(&output.stderr));
    }
}
