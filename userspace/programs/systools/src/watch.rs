// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! watch(1): runs a command again and again, showing its output full
//! screen, as procps-ng's watch does: `-n` seconds apart, `-d` marking
//! what changed, `-g` stopping when it changes, `-e` on an error.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::args::{Arg, Args};
use crate::util::{self, Fail, fail, usage};

/// Set by SIGINT, SIGTERM or SIGHUP.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// Runs the command once: its output and errors, as they came, and how
/// it ended.
fn run(command: &[OsString], exec: bool) -> io::Result<(Vec<u8>, i32)> {
    let mut cmd = if exec {
        let mut cmd = Command::new(&command[0]);
        cmd.args(&command[1..]);
        cmd
    } else {
        // The words joined, for the shell, as watch does.
        let line: Vec<String> = command.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(line.join(" "));
        cmd
    };
    // One pipe for both, so that neither fills while the other is read.
    let (mut reader, writer) = io::pipe()?;
    let errors = writer.try_clone()?;
    let mut child = cmd.stdin(Stdio::null()).stdout(writer).stderr(errors).spawn()?;
    // The command's copies of the pipe go, so that its end is the end.
    drop(cmd);
    let mut output = Vec::new();
    reader.read_to_end(&mut output)?;
    let status = child.wait()?;
    Ok((output, status.code().unwrap_or(128)))
}

/// The output as screen lines: tabs expanded, control characters dropped,
/// each cut to `width` columns, at most `rows` of them.
fn screen_lines(output: &[u8], width: usize, rows: usize) -> Vec<Vec<char>> {
    let text = String::from_utf8_lossy(output);
    let mut lines = Vec::new();
    for line in text.split('\n') {
        if lines.len() >= rows {
            break;
        }
        let mut cells = Vec::new();
        for c in line.chars() {
            match c {
                '\t' => {
                    let next = (cells.len() / 8 + 1) * 8;
                    cells.resize(next, ' ');
                }
                c if c.is_control() => {}
                c => cells.push(c),
            }
        }
        cells.truncate(width);
        lines.push(cells);
    }
    if lines.last().is_some_and(Vec::is_empty) {
        lines.pop();
    }
    lines
}

/// watch(1).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    args.stop_at_operand();
    let mut interval = 2.0f64;
    let mut differences = false;
    let mut cumulative = false;
    let mut title = true;
    let mut until_change = false;
    let mut error_exit = false;
    let mut beep = false;
    let mut exec = false;
    let mut precise = false;
    let mut command: Vec<OsString> = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('n') => interval = seconds(&args.text()?)?,
            Arg::Long(l) if l == "interval" => interval = seconds(&args.text()?)?,
            Arg::Short('d') => {
                differences = true;
                cumulative = args.joined_value().is_some_and(|v| v == "permanent");
            }
            Arg::Long(l) if l == "differences" => {
                differences = true;
                cumulative = args.optional_value().is_some_and(|v| v == "permanent");
            }
            Arg::Short('t') => title = false,
            Arg::Long(l) if l == "no-title" => title = false,
            Arg::Short('g') => until_change = true,
            Arg::Long(l) if l == "chgexit" => until_change = true,
            Arg::Short('e') => error_exit = true,
            Arg::Long(l) if l == "errexit" => error_exit = true,
            Arg::Short('b') => beep = true,
            Arg::Long(l) if l == "beep" => beep = true,
            Arg::Short('x') => exec = true,
            Arg::Long(l) if l == "exec" => exec = true,
            Arg::Short('p') => precise = true,
            Arg::Long(l) if l == "precise" => precise = true,
            Arg::Short('c' | 'w' | 'C') => {}
            Arg::Long(l) if l == "color" || l == "no-wrap" || l == "no-color" => {}
            Arg::Operand(word) => {
                command.push(word);
                command.extend(args.rest());
            }
            other => return Err(args.unknown(&other)),
        }
    }
    if command.is_empty() {
        return Err(usage("usage: watch [-bdegptx] [-n SECONDS] COMMAND".into()));
    }
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: all zeros is a valid sigaction: numbers, a signal set and a function address.
        let mut action: libc::sigaction = unsafe { util::zeroed() };
        action.sa_sigaction = on_signal as *const () as usize;
        // SAFETY: the handler only stores to an atomic.
        unsafe { libc::sigaction(signal, &raw const action, std::ptr::null_mut()) };
    }
    let terminal = util::is_terminal(1);
    let mut out = io::stdout().lock();
    if terminal {
        write!(out, "\x1b[?1049h\x1b[?25l")?;
    }
    let shown: Vec<String> = command.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let shown = shown.join(" ");
    let host = util::hostname();
    let mut last: Option<Vec<Vec<char>>> = None;
    let mut changed_ever: Vec<Vec<bool>> = Vec::new();
    let mut code = 0;
    let started = Instant::now();
    let mut round = 0u32;
    loop {
        let begun = Instant::now();
        let (width, height) = util::terminal_size(1).unwrap_or((80, 24));
        let (output, status) = match run(&command, exec) {
            Ok(result) => result,
            Err(e) => {
                code = 1;
                eprintln!("watch: {}: {}", shown, util::strip_os_error(&e));
                break;
            }
        };
        let body_rows = if title { height.saturating_sub(2) } else { height };
        let lines = screen_lines(&output, width, body_rows);
        let mut screen = String::new();
        if terminal {
            screen.push_str("\x1b[H\x1b[J");
        }
        if title {
            let now = (util::now_ns() / 1_000_000_000) as i64;
            let left = format!("Every {interval:.1}s: {shown}");
            let right = format!("{host}: {}", util::local_time(now, "%a %b %e %H:%M:%S %Y"));
            let room = width.saturating_sub(right.chars().count() + 1);
            let left: String = left.chars().take(room).collect();
            let pad = width.saturating_sub(left.chars().count() + right.chars().count());
            screen.push_str(&format!("{left}{}{right}\n\n", " ".repeat(pad)));
        }
        if changed_ever.len() < lines.len() {
            changed_ever.resize(lines.len(), Vec::new());
        }
        for (row, line) in lines.iter().enumerate() {
            if differences && let Some(previous) = &last {
                let old = previous.get(row);
                let marks = &mut changed_ever[row];
                if marks.len() < line.len() {
                    marks.resize(line.len(), false);
                }
                for (col, c) in line.iter().enumerate() {
                    let differs = old.and_then(|o| o.get(col)) != Some(c);
                    if differs {
                        marks[col] = true;
                    }
                    let mark = if cumulative { marks[col] } else { differs };
                    if mark && terminal {
                        screen.push_str(&format!("\x1b[7m{c}\x1b[0m"));
                    } else {
                        screen.push(*c);
                    }
                }
            } else {
                screen.extend(line.iter());
            }
            screen.push('\n');
        }
        let screen = if terminal { screen.replace('\n', "\r\n") } else { screen };
        out.write_all(screen.as_bytes())?;
        out.flush()?;
        if status != 0 && beep {
            out.write_all(b"\x07")?;
        }
        if status != 0 && error_exit {
            // Frozen on the output, as procps's watch is, until a key.
            let (_, height) = util::terminal_size(1).unwrap_or((80, 24));
            if terminal {
                write!(out, "\x1b[{height};1H")?;
            }
            write!(out, "command exit with a non-zero status, press a key to exit")?;
            if !terminal {
                writeln!(out)?;
            }
            out.flush()?;
            wait_for_key();
            code = 8;
            break;
        }
        let changed = last.as_ref().is_some_and(|l| *l != lines);
        last = Some(lines);
        if until_change && changed {
            break;
        }
        round += 1;
        // Wait out the interval: from the start of the run with -p, else
        // from its end.
        let wait = Duration::from_secs_f64(interval);
        let deadline =
            if precise { started + wait * round } else { begun.max(Instant::now()) + wait };
        while Instant::now() < deadline && !STOP.load(Ordering::Relaxed) {
            std::thread::sleep((deadline - Instant::now()).min(Duration::from_millis(100)));
        }
        if STOP.load(Ordering::Relaxed) {
            break;
        }
    }
    if terminal {
        write!(out, "\x1b[?25h\x1b[?1049l")?;
        out.flush()?;
    }
    if code == 1 {
        return Err(fail(String::new()));
    }
    Ok(code)
}

/// Waits for a key on standard input, a terminal read a key at a time;
/// returns at once if it is not a terminal, or on a signal.
fn wait_for_key() {
    // SAFETY: all zeros is a valid termios: numbers.
    let mut saved: libc::termios = unsafe { util::zeroed() };
    // SAFETY: tcgetattr fills `saved`.
    let tty = unsafe { libc::tcgetattr(0, &raw mut saved) } == 0;
    if !tty {
        return;
    }
    let mut keys = saved;
    keys.c_lflag &= !(libc::ICANON | libc::ECHO);
    keys.c_cc[libc::VMIN] = 1;
    keys.c_cc[libc::VTIME] = 0;
    let mut byte = 0u8;
    // SAFETY: valid termios structures and a one-byte buffer of ours.
    unsafe {
        libc::tcsetattr(0, libc::TCSANOW, &raw const keys);
        libc::read(0, (&raw mut byte).cast(), 1);
        libc::tcsetattr(0, libc::TCSANOW, &raw const saved);
    }
}

/// An interval in seconds, at least 0.1, with a point or a comma.
fn seconds(text: &str) -> Result<f64, Fail> {
    let seconds: f64 = text
        .replace(',', ".")
        .parse()
        .map_err(|_| usage(format!("failed to parse argument: '{text}'")))?;
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(usage(format!("failed to parse argument: '{text}'")));
    }
    Ok(seconds.max(0.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_fits_the_screen() {
        let lines = screen_lines(b"a\tb\nlong line\nthird\n", 6, 2);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].iter().collect::<String>(), "a     ");
        assert_eq!(lines[1].iter().collect::<String>(), "long l");
        assert_eq!(seconds("0,5").unwrap(), 0.5);
        assert_eq!(seconds("0").unwrap(), 0.1);
        assert!(seconds("x").is_err());
    }
}
