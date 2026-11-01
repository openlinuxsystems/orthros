// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Asking Nebix, the local assistant (ADRs 0268 and 0280). Nebix lands
//! in 1.10; until then its socket is not there and every question here
//! answers nothing at once, so the shell's plain answers stand alone.
//!
//! Nebix runs on this computer and is asked over its socket only, a JSON
//! line each way; nothing here reaches the network. What is sent is
//! masked first (`explain::mask`), and a question that takes more than
//! 1.5 seconds is given up on.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Nebix's socket.
pub const SOCKET: &str = "/system/live/nebix/socket";

/// Sends `request` and reads one line back.
fn ask(request: &str, wait: Duration) -> Option<String> {
    if !std::path::Path::new(SOCKET).exists() {
        return None;
    }
    let mut socket = UnixStream::connect(SOCKET).ok()?;
    socket.set_write_timeout(Some(wait)).ok()?;
    socket.set_read_timeout(Some(wait)).ok()?;
    socket.write_all(request.as_bytes()).ok()?;
    socket.write_all(b"\n").ok()?;
    let mut answer = String::new();
    BufReader::new(socket.take(64 * 1024)).read_line(&mut answer).ok()?;
    let answer = answer.trim();
    (!answer.is_empty()).then(|| answer.to_string())
}

/// The `text` of an answer such as `{"text":"..."}`; an answer that is
/// not JSON is the text itself.
fn text_of(answer: &str) -> String {
    let Some(rest) = answer.trim().strip_prefix("{\"text\":\"") else { return answer.to_string() };
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('u') => {
                    let code: String = chars.by_ref().take(4).collect();
                    if let Some(c) = u32::from_str_radix(&code, 16).ok().and_then(char::from_u32) {
                        out.push(c);
                    }
                }
                Some(other) => out.push(other),
                None => break,
            },
            c => out.push(c),
        }
    }
    out
}

/// What Nebix says `line` does, or why it failed with `status`.
pub fn explain(line: &str, status: Option<i32>) -> Option<String> {
    let status = status.map_or_else(|| "null".to_string(), |s| s.to_string());
    let request = format!("{{\"explain\":{},\"status\":{status}}}", crate::notify::json(line));
    ask(&request, Duration::from_millis(1500)).map(|a| text_of(&a))
}

/// The command Nebix thinks comes after `last` (masked) in `folder`,
/// given the record's own guess; at most 300 ms, as it is asked at the
/// prompt.
pub fn next(last: &str, status: i32, folder: &str, guess: Option<&str>) -> Option<String> {
    let guess = guess.map_or_else(|| "null".to_string(), crate::notify::json);
    let request = format!(
        "{{\"next\":{},\"status\":{status},\"folder\":{},\"guess\":{guess}}}",
        crate::notify::json(last),
        crate::notify::json(folder)
    );
    let answer = text_of(&ask(&request, Duration::from_millis(300))?);
    // One line, and one command: never more than a hint.
    let first = answer.lines().next()?.trim().to_string();
    (!first.is_empty()).then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_read() {
        assert_eq!(text_of("{\"text\":\"a\\nb \\u00e9\"}"), "a\nb é");
        assert_eq!(text_of("plain"), "plain");
    }
}
