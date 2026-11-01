// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `ob bench --shell` (ADR 0266): how long Nukesh takes to start and to
//! come back to its prompt, next to bash, both on this machine and each
//! at a terminal of its own.
//!
//! Each shell runs interactively on a pseudo-terminal that script(1)
//! makes, with an empty home (no startup files) and
//! `TERM=xterm-256color`. Startup is from starting it until its first
//! prompt is on the terminal; the round trip, from `true` and Enter typed
//! until the next prompt is. Each is the fastest of [`RUNS`]. Nukesh
//! draws its prompt, highlighting and hints as it would for a person;
//! bash runs with readline, as it would. Both run on the same kernel and
//! under the same script(1), so the numbers compare the shells alone.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use crate::{Ctx, Error, Result};

/// How many shells of each are started; the fastest counts.
const RUNS: u32 = 20;

/// Round trips timed in each.
const ROUNDS: u32 = 5;

/// The longest a shell may take to show a prompt.
const LIMIT: Duration = Duration::from_secs(10);

fn io(what: &str, err: &std::io::Error) -> Error {
    Error::new(format!("{what}: {err}"))
}

/// A shell at a terminal.
struct Session {
    child: Child,
    input: ChildStdin,
    output: Receiver<Vec<u8>>,
    seen: Vec<u8>,
}

impl Session {
    fn start(program: &Path, args: &[&str], home: &Path) -> Result<Session> {
        let line = std::iter::once(program.display().to_string())
            .chain(args.iter().map(|a| (*a).to_string()))
            .collect::<Vec<_>>()
            .join(" ");
        let mut child = Command::new("script")
            .args(["-q", "-f", "-e", "-c", &line, "/dev/null"])
            .env_clear()
            .env("HOME", home)
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .env("PS1", "$ ")
            .env("LANG", "C.UTF-8")
            .current_dir(home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| io("script", &e))?;
        let input = child.stdin.take().ok_or_else(|| Error::new("script: no input"))?;
        let mut out = child.stdout.take().ok_or_else(|| Error::new("script: no output"))?;
        let (send, output) = channel();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            while let Ok(n) = out.read(&mut buffer) {
                if n == 0 || send.send(buffer[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        Ok(Session { child, input, output, seen: Vec::new() })
    }

    /// Reads until what came after `from` ends in a prompt and `want`
    /// came before it, answering the terminal's questions on the way, as
    /// a terminal would.
    fn wait_prompt(&mut self, from: usize, want: &str) -> Result<()> {
        let started = Instant::now();
        loop {
            let after = &self.seen[from..];
            let text = plain(after);
            if let Some(at) = text.find(want) {
                let rest = &text[at + want.len()..];
                if (want.is_empty() || rest.contains('\n')) && ends_in_prompt(rest) {
                    return Ok(());
                }
            }
            let left = LIMIT.checked_sub(started.elapsed()).unwrap_or_default();
            match self.output.recv_timeout(left) {
                Ok(new) => {
                    // Where is the cursor? (reedline asks.)
                    let asks = new.windows(4).filter(|w| *w == b"\x1b[6n").count();
                    for _ in 0..asks {
                        self.input.write_all(b"\x1b[1;1R").map_err(|e| io("script", &e))?;
                    }
                    self.seen.extend_from_slice(&new);
                }
                Err(RecvTimeoutError::Timeout) => crate::bail!(
                    "no prompt after {}s; the terminal shows {:?}",
                    LIMIT.as_secs(),
                    plain(&self.seen[self.seen.len().saturating_sub(300)..])
                ),
                Err(RecvTimeoutError::Disconnected) => crate::bail!("the shell left"),
            }
        }
    }

    /// Takes what came, without waiting.
    fn drain(&mut self) {
        while let Ok(new) = self.output.recv_timeout(Duration::from_millis(30)) {
            self.seen.extend_from_slice(&new);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The text of what a terminal got, without escapes or carriage returns.
fn plain(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: to its final byte.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: to BEL.
                Some(']') => {
                    for c in chars.by_ref() {
                        if c == '\x07' {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

/// Whether the last line of `text` is a prompt: it has `$ ` or `# `
/// (Nukesh's right prompt may follow).
fn ends_in_prompt(text: &str) -> bool {
    let last = text.rsplit('\n').next().unwrap_or("");
    last.starts_with("$ ") || last.starts_with("# ") || last.contains(" $ ") || last.contains(" # ")
}

/// The fastest startup and round trip of `program`.
fn measure(program: &Path, args: &[&str], home: &Path) -> Result<(Duration, Duration)> {
    let mut startup = Duration::MAX;
    let mut round = Duration::MAX;
    for _ in 0..RUNS {
        let started = Instant::now();
        let mut session = Session::start(program, args, home)?;
        session.wait_prompt(0, "")?;
        startup = startup.min(started.elapsed());
        session.drain();
        for _ in 0..ROUNDS {
            let from = session.seen.len();
            let started = Instant::now();
            session.input.write_all(b"true\r").map_err(|e| io("script", &e))?;
            session.wait_prompt(from, "true")?;
            round = round.min(started.elapsed());
            session.drain();
        }
    }
    Ok((startup, round))
}

fn find_bash() -> Option<PathBuf> {
    ["/usr/bin/bash", "/bin/bash", "/usr/local/bin/bash"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
}

fn shown(d: Duration) -> String {
    format!("{:.2} ms", d.as_secs_f64() * 1e3)
}

/// `ob bench --shell`.
pub fn command(ctx: &Ctx, summary: Option<&str>) -> Result<()> {
    let Some(bash) = find_bash() else {
        crate::bail!("bench --shell compares with bash, which is not installed")
    };
    eprintln!("    Building nukesh for this machine");
    let mut build = crate::userspace::host_cargo(ctx, "build");
    build.args(["--release", "-p", "nukesh"]);
    crate::process::run(ctx, &mut build)?;
    let nukesh = ctx.target.join("programs-host/release/nukesh");
    let dir = ctx.out.join("bench");
    let home = dir.join("shell-home");
    let _ = std::fs::remove_dir_all(&home);
    crate::process::create_dir(&home)?;
    eprintln!("    Timing nukesh and bash: {RUNS} starts and {} round trips each", RUNS * ROUNDS);
    let (n_start, n_round) = measure(&nukesh, &["-i"], &home)?;
    let (b_start, b_round) = measure(&bash, &["--norc", "--noprofile", "-i"], &home)?;
    let ratio = |a: Duration, b: Duration| a.as_secs_f64() / b.as_secs_f64();
    let report = format!(
        "| Shell, on this machine | Nukesh | bash | Ratio |\n|---|---:|---:|---:|\n\
         | Started, to its first prompt | {} | {} | {:.2} |\n\
         | `true` and Enter, to the next prompt | {} | {} | {:.2} |\n",
        shown(n_start),
        shown(b_start),
        ratio(n_start, b_start),
        shown(n_round),
        shown(b_round),
        ratio(n_round, b_round)
    );
    print!("{report}");
    std::fs::write(dir.join("shell.md"), &report).map_err(|e| io("out/bench/shell.md", &e))?;
    if let Some(path) = summary {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .map_err(|e| io(path, &e))?;
        file.write_all(format!("\n{report}").as_bytes()).map_err(|e| io(path, &e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_are_seen_through_escapes() {
        assert!(ends_in_prompt(&plain(b"user@host ~ $ ")));
        assert!(ends_in_prompt(&plain(b"\x1b[32muser\x1b[0m \x1b[35m$\x1b[0m \x1b[K")));
        assert!(ends_in_prompt(&plain(b"root@vm ~ \x1b[43m admin \x1b[0m # \x1b[50Ctook 4.2s")));
        assert!(!ends_in_prompt(&plain(b"$ true\r\n")));
    }
}
