// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The scripts of `tests/cases`, each run by the shell and its standard
//! output and exit status compared with its `.out` file: what bash gives
//! for it, or dash for the `posix-*` ones, which run as `sh`. The boot
//! test runs the same scripts on Radix (userspace/posix/shell.c).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a script may take.
const TIMEOUT: Duration = Duration::from_secs(60);

fn cases() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cases")
}

/// The script's standard output and `status=N`, as the `.out` files have
/// them.
fn run(shell: &Path, script: &Path) -> String {
    let mut child = Command::new(shell)
        .arg(script)
        .current_dir("/tmp")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", "/root")
        .env("TERM", "dumb")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("starting the shell");
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut text = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut text).unwrap();
        text
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > TIMEOUT {
            let _ = child.kill();
            panic!("{} took longer than {TIMEOUT:?}", script.display());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let text = String::from_utf8_lossy(&reader.join().unwrap()).into_owned();
    let code = status.code().unwrap_or(128);
    format!("{text}status={code}\n")
}

#[test]
fn scripts_give_what_bash_and_dash_give() {
    let nukesh = PathBuf::from(env!("CARGO_BIN_EXE_nukesh"));
    // `sh` for the POSIX ones: a link of that name, as /bin/sh is.
    let links = std::env::temp_dir().join(format!("nukesh-cases-{}", std::process::id()));
    std::fs::create_dir_all(&links).unwrap();
    let sh = links.join("sh");
    let _ = std::fs::remove_file(&sh);
    std::os::unix::fs::symlink(&nukesh, &sh).unwrap();
    let mut scripts: Vec<PathBuf> = std::fs::read_dir(cases())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|e| e == "sh"))
        .collect();
    scripts.sort();
    assert!(!scripts.is_empty(), "no scripts in {}", cases().display());
    let mut failed = Vec::new();
    for script in &scripts {
        let name = script.file_stem().unwrap().to_string_lossy().into_owned();
        let shell = if name.starts_with("posix-") { &sh } else { &nukesh };
        let expected = std::fs::read_to_string(script.with_extension("out")).unwrap();
        let got = run(shell, script);
        if got != expected {
            let line = expected
                .lines()
                .zip(got.lines())
                .position(|(a, b)| a != b)
                .unwrap_or(expected.lines().count().min(got.lines().count()));
            failed.push(format!(
                "{name}: line {} of its output: expected {:?}, got {:?}",
                line + 1,
                expected.lines().nth(line).unwrap_or("(the end)"),
                got.lines().nth(line).unwrap_or("(the end)")
            ));
        }
    }
    let _ = std::fs::remove_dir_all(&links);
    assert!(
        failed.is_empty(),
        "{} of {} scripts differ:\n{}",
        failed.len(),
        scripts.len(),
        failed.join("\n")
    );
}
