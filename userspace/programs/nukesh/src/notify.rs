// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A long command that ends while you are on another console says so
//! (ADR 0263): when a command typed at an interactive shell ran for
//! colors.toml's `[notify] after` seconds or more, the shell sends
//! notifyd(8) a notification naming the command, how it ended and how
//! long it took, with the shell's terminal. consoled(8) does not show it
//! while that terminal's console is the one on the screen: you saw it
//! end. Programs you work in while they run, such as an editor or a
//! pager, never send one.

use std::io::Write;

/// notifyd's socket.
const SOCKET: &str = "/system/live/notify/socket";

/// Programs that are used, not waited for.
const INTERACTIVE: &[&str] = &[
    "vi", "vim", "nvim", "nano", "micro", "hx", "emacs", "less", "more", "most", "man", "info",
    "top", "htop", "btop", "watch", "tmux", "screen", "ssh", "mosh", "nukesh", "bash", "sh", "zsh",
    "fish", "python", "python3", "node", "irb", "sqlite3", "psql", "mysql", "ftp", "sftp",
    "telnet", "nc", "journal", "fg",
];

/// Whether `command` is worth a notification.
pub fn worth_telling(command: &str) -> bool {
    let first = command
        .split_whitespace()
        .find(|w| !w.contains('=') && !matches!(*w, "sudo" | "elevate" | "doas" | "time" | "exec"))
        .unwrap_or("");
    let name = first.rsplit('/').next().unwrap_or(first);
    !name.is_empty() && !INTERACTIVE.contains(&name)
}

/// The terminal standard input is, as `pts/3` or `tty2`.
pub fn terminal() -> Option<String> {
    let mut buffer = [0u8; 128];
    // SAFETY: the buffer is ours and its length is given.
    let failed = unsafe { libc::ttyname_r(0, buffer.as_mut_ptr().cast(), buffer.len()) };
    if failed != 0 {
        return None;
    }
    let end = buffer.iter().position(|&b| b == 0)?;
    let path = std::str::from_utf8(&buffer[..end]).ok()?;
    let name = path.strip_prefix("/devices/").or_else(|| path.strip_prefix("/dev/"))?;
    Some(name.to_string())
}

fn json(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The request, as a line of JSON.
pub fn request(command: &str, status: i32, millis: u64, terminal: Option<&str>) -> String {
    let shown: String = command.replace('\n', " ").chars().take(60).collect();
    let how = if status == 0 { "finished".to_string() } else { format!("failed with status {status}") };
    let title = format!("{shown} {how}");
    let mut body = format!("after {}", crate::record::duration(millis));
    if let Some(terminal) = terminal {
        body.push_str(&format!(" on {terminal}"));
    }
    let mut line = format!(
        "{{\"ask\":\"send\",\"title\":{},\"body\":{},\"app\":\"nukesh\"",
        json(&title),
        json(&body)
    );
    if let Some(terminal) = terminal {
        line.push_str(&format!(",\"terminal\":{}", json(terminal)));
    }
    line.push_str("}\n");
    line
}

/// After a command: tells notifyd when it was long enough.
pub fn after_command(shell: &crate::shell::Shell, command: &str, status: i32, millis: u64) {
    let colors = &shell.colors;
    let after = colors.number("notify.after").saturating_mul(1000);
    if !colors.on("notify") || after == 0 || millis < after || !worth_telling(command) {
        return;
    }
    let Ok(mut socket) = std::os::unix::net::UnixStream::connect(SOCKET) else { return };
    let _ = socket.set_write_timeout(Some(std::time::Duration::from_millis(200)));
    let _ = socket.write_all(request(command, status, millis, terminal().as_deref()).as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editors_and_pagers_are_not_waited_for() {
        assert!(worth_telling("make -j8"));
        assert!(worth_telling("sudo apt upgrade"));
        assert!(!worth_telling("vim notes.txt"));
        assert!(!worth_telling("LESS=R /usr/bin/less x"));
        assert!(!worth_telling(""));
    }

    #[test]
    fn the_request_is_a_line_of_json() {
        let line = request("make \"all\"", 2, 95_000, Some("pts/1"));
        assert_eq!(
            line,
            "{\"ask\":\"send\",\"title\":\"make \\\"all\\\" failed with status 2\",\"body\":\"after 1m 35s on pts/1\",\"app\":\"nukesh\",\"terminal\":\"pts/1\"}\n"
        );
    }
}
