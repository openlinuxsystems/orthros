// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Passwords asked on the consoles, and the Ctrl+Alt+Delete menu
//! (ADR 0251).
//!
//! consoled asks authd(8) `prompts`, and keeps that connection open:
//! authd writes on it an `ask` line each time elevate(8), in a pane on
//! one of the consoles, needs its person's password. consoled shows the
//! prompt on that console's bottom line, takes the keys typed there
//! itself, never handing them to the console's programs, and sends what
//! was typed to authd alone, on a connection of its own. So no program
//! draws the prompt or reads the password, and Ctrl+Alt+Delete, which no
//! program ever receives, brings it back cleared: what is typed after it
//! goes to consoled. A terminal that is not a console's is answered
//! `elsewhere`, and elevate then asks itself. While authd is not running,
//! consoled tries again every 5 seconds.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use accounts::request::{Answer, Prompt, Prompted, Request};

use crate::notices::connect_now;
use crate::term::{self, BOLD, Cell, REVERSE, WIDE_TAIL};

/// How long consoled waits to try authd again.
const RETRY: Duration = Duration::from_secs(5);

/// The most bytes of a line from authd; a longer one is dropped.
const LINE_MAX: usize = 4096;

/// The most bytes of a password typed at a prompt, as authd reads no
/// longer request.
const TYPED_MAX: usize = 1024;

/// The bottom line's colours while consoled asks: black on cyan, bold, so
/// that it looks like no notification.
const FG: u8 = 0;
const BG: u8 = 6;

/// A password asked for on a console.
pub struct Ask {
    /// authd's number for it.
    id: u64,
    /// The console's index.
    console: usize,
    /// Whose password it is.
    name: String,
    /// The command line it is for; empty to unlock elevate.
    shown: String,
    /// What was typed so far.
    typed: Vec<u8>,
}

impl Drop for Ask {
    fn drop(&mut self) {
        accounts::wipe(&mut self.typed);
    }
}

impl Ask {
    /// The prompt's words.
    fn text(&self) -> (String, String) {
        let lead = format!("[elevate] Password for {}, to ", self.name);
        if self.shown.is_empty() {
            (format!("{lead}unlock elevate in this pane"), String::new())
        } else {
            (format!("{lead}run "), self.shown.clone())
        }
    }

    /// The bottom line's `cols` cells: the prompt, the command cut with
    /// `...` if it does not fit, `:` and a block where the password goes,
    /// which never shows.
    pub fn cells(&self, cols: usize) -> Vec<Cell> {
        let (lead, command) = self.text();
        let blank = Cell { c: ' ', fg: FG, bg: BG, flags: BOLD };
        let widths = |text: &str| -> Vec<(char, usize)> {
            text.chars()
                .map(|c| if c.is_control() { '?' } else { c })
                .map(|c| (c, term::width(c)))
                .filter(|&(_, width)| width > 0)
                .collect()
        };
        let lead = widths(&lead);
        let mut command = widths(&command);
        // A space in front, `: ` and the block after.
        let room = cols.saturating_sub(4);
        let width = |chars: &[(char, usize)]| chars.iter().map(|&(_, w)| w).sum::<usize>();
        if width(&lead) + width(&command) > room {
            let left = room.saturating_sub(width(&lead) + 3);
            let mut kept = 0;
            command.retain(|&(_, w)| {
                kept += w;
                kept <= left
            });
            command.extend([('.', 1); 3]);
        }
        let mut cells = vec![blank];
        for (c, width) in lead.into_iter().chain(command).chain([(':', 1), (' ', 1)]) {
            if cells.len() + width > cols {
                break;
            }
            cells.push(Cell { c, ..blank });
            if width == 2 {
                cells.push(Cell { c: ' ', flags: BOLD | WIDE_TAIL, ..blank });
            }
        }
        if cells.len() < cols {
            cells.push(Cell { flags: BOLD | REVERSE, ..blank });
        }
        cells.resize(cols, blank);
        cells.truncate(cols);
        cells
    }
}

/// What a key typed at a prompt did.
#[derive(Debug, PartialEq, Eq)]
pub enum Typed {
    /// No prompt waits on that console: the key is the console's.
    NotTaken,
    /// The prompt took it.
    Taken,
}

/// consoled's connection to authd for prompts, and the prompts asked.
pub struct Prompts {
    stream: Option<UnixStream>,
    /// A line read in part.
    pending: Vec<u8>,
    /// When to try authd again.
    retry_at: Option<Instant>,
    /// Whether consoled said authd went away, so it says so once.
    said_gone: bool,
    /// Whether authd's answer to `prompts` came.
    answered: bool,
    /// The prompts, oldest first: a console shows its oldest.
    asks: Vec<Ask>,
}

impl Prompts {
    /// No connection yet: the first try is at once.
    pub fn new(now: Instant) -> Prompts {
        Prompts {
            stream: None,
            pending: Vec::new(),
            retry_at: Some(now),
            said_gone: false,
            answered: false,
            asks: Vec::new(),
        }
    }

    /// The connection's descriptor, to poll; -1 for none.
    pub fn fd(&self) -> i32 {
        self.stream.as_ref().map_or(-1, AsRawFd::as_raw_fd)
    }

    /// When to try authd again.
    pub fn deadline(&self) -> Option<Instant> {
        self.retry_at
    }

    /// Tries authd again if it is time.
    pub fn tick(&mut self, now: Instant) {
        if self.retry_at.is_some_and(|at| at <= now) {
            self.connect(now);
        }
    }

    fn connect(&mut self, now: Instant) {
        let connected = connect_now(accounts::SOCKET).and_then(|mut stream| {
            stream.write_all(&Request::Prompts.encode())?;
            stream.shutdown(Shutdown::Write)?;
            Ok(stream)
        });
        match connected {
            Ok(stream) => {
                self.stream = Some(stream);
                self.retry_at = None;
                self.answered = false;
            }
            Err(_) => self.retry_at = Some(now + RETRY),
        }
    }

    fn lost(&mut self, now: Instant, why: &str) {
        if !self.said_gone {
            eprintln!("consoled: lost authd's prompts ({why}); trying again every 5 seconds");
            self.said_gone = true;
        }
        self.stream = None;
        self.pending.clear();
        // authd has elevate ask for these itself now.
        self.asks.clear();
        self.retry_at = Some(now + RETRY);
    }

    /// Reads what authd sent; `console_of` gives the console of a terminal
    /// named as `pts/N`.
    pub fn read(&mut self, now: Instant, console_of: impl Fn(&str) -> Option<usize>) {
        let mut buffer = [0u8; 4096];
        loop {
            let Some(stream) = self.stream.as_mut() else { return };
            match stream.read(&mut buffer) {
                Ok(0) => return self.lost(now, "it closed the connection"),
                Ok(n) => self.take(&buffer[..n], now, &console_of),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return,
                Err(err) => return self.lost(now, &err.to_string()),
            }
        }
    }

    /// Takes in `bytes` from authd.
    fn take(&mut self, bytes: &[u8], now: Instant, console_of: &impl Fn(&str) -> Option<usize>) {
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line).into_owned();
            if !self.answered {
                match Answer::decode(&line) {
                    Answer::Done(_) => {
                        eprintln!("consoled: asking for passwords on the consoles for authd");
                        self.answered = true;
                        self.said_gone = false;
                    }
                    Answer::Refused(why) => return self.lost(now, &why),
                }
                continue;
            }
            match Prompt::decode(&line) {
                Some(Prompt::Ask { id, terminal, name, shown }) => match console_of(&terminal) {
                    Some(console) => {
                        self.asks.push(Ask { id, console, name, shown, typed: Vec::new() });
                    }
                    None => answer(id, Prompted::Elsewhere),
                },
                Some(Prompt::Done { id }) => self.asks.retain(|ask| ask.id != id),
                None => {}
            }
        }
        if self.pending.len() > LINE_MAX {
            self.pending.clear();
        }
    }

    /// The prompt console `console` shows, if one waits there.
    pub fn shown(&self, console: usize) -> Option<&Ask> {
        self.asks.iter().find(|ask| ask.console == console)
    }

    /// Clears what was typed at console `console`'s prompt, for
    /// Ctrl+Alt+Delete. Whether one waits there.
    pub fn clear(&mut self, console: usize) -> bool {
        match self.asks.iter_mut().find(|ask| ask.console == console) {
            Some(ask) => {
                accounts::wipe(&mut ask.typed);
                ask.typed.clear();
                true
            }
            None => false,
        }
    }

    /// Bytes a key sent on console `console`: the prompt's there, if one
    /// waits. Enter answers it, Esc or Ctrl+C closes it, Backspace takes
    /// back a character and Ctrl+U all of them; other keys' sequences go
    /// nowhere.
    pub fn key(&mut self, console: usize, bytes: &[u8]) -> Typed {
        let Some(at) = self.asks.iter().position(|ask| ask.console == console) else {
            return Typed::NotTaken;
        };
        let ask = &mut self.asks[at];
        match bytes {
            b"\r" | b"\n" => {
                let ask = self.asks.remove(at);
                answer(ask.id, Prompted::Password(ask.typed.clone()));
            }
            b"\x1b" | b"\x03" => {
                let ask = self.asks.remove(at);
                answer(ask.id, Prompted::Cancelled);
            }
            b"\x7f" | b"\x08" => {
                // A whole character, however many bytes it took.
                while let Some(byte) = ask.typed.pop() {
                    if byte & 0xc0 != 0x80 {
                        break;
                    }
                }
            }
            b"\x15" => {
                accounts::wipe(&mut ask.typed);
                ask.typed.clear();
            }
            [0x1b, ..] => {}
            // Text, as much as authd takes.
            text if text.iter().all(|&b| b >= 0x20 && b != 0x7f)
                && ask.typed.len() + text.len() <= TYPED_MAX =>
            {
                ask.typed.extend_from_slice(text);
            }
            _ => {}
        }
        Typed::Taken
    }
}

/// Sends authd what came of prompt `id`, on a connection of its own, not
/// waiting for its answer.
fn answer(id: u64, typed: Prompted) {
    let mut bytes = Request::Prompted { id, typed }.encode();
    let sent = connect_now(accounts::SOCKET).and_then(|mut stream| {
        stream.write_all(&bytes)?;
        stream.shutdown(Shutdown::Write)
    });
    accounts::wipe(&mut bytes);
    if let Err(err) = sent {
        eprintln!("consoled: cannot answer authd's prompt {id}: {err}");
    }
}

/// The Ctrl+Alt+Delete menu on the bottom line, consoled's own: `sign_in`
/// says whether Enter signs in.
pub fn menu_cells(cols: usize, sign_in: bool) -> Vec<Cell> {
    let text = if sign_in {
        "Ctrl+Alt+Delete    Enter: sign in    R: restart    P: power off    Esc: back"
    } else {
        "Ctrl+Alt+Delete    R: restart    P: power off    Esc: back"
    };
    note_cells(cols, text)
}

/// `text` on the bottom line, in the colours of consoled's own prompts.
pub fn note_cells(cols: usize, text: &str) -> Vec<Cell> {
    let blank = Cell { c: ' ', fg: FG, bg: BG, flags: BOLD };
    let mut cells = vec![blank];
    cells.extend(text.chars().map(|c| Cell { c, ..blank }));
    cells.resize(cols, blank);
    cells.truncate(cols);
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(cells: &[Cell]) -> String {
        cells.iter().filter(|cell| cell.flags & WIDE_TAIL == 0).map(|cell| cell.c).collect()
    }

    fn ask(console: usize, shown: &str) -> Ask {
        Ask { id: 3, console, name: "alice".into(), shown: shown.into(), typed: Vec::new() }
    }

    #[test]
    fn the_prompt_fits_the_line() {
        let cells = ask(0, "/system/commands/id").cells(80);
        assert_eq!(cells.len(), 80);
        let shown = text(&cells);
        assert!(
            shown.starts_with(" [elevate] Password for alice, to run /system/commands/id:  "),
            "{shown}"
        );
        // The block where the password goes, after `: `.
        let at = shown.find(": ").unwrap() + 2;
        assert_eq!(cells[at].flags & REVERSE, REVERSE);
        assert_eq!((cells[1].fg, cells[1].bg), (FG, BG));
        // A long command is cut, keeping the block.
        let long = ask(0, &format!("/system/commands/echo {}", "x".repeat(200))).cells(80);
        let shown = text(&long);
        assert!(shown.contains("echo xx"), "{shown}");
        assert!(shown.contains("...:"), "{shown}");
        assert!(long.iter().any(|cell| cell.flags & REVERSE != 0));
        // An unlock names no command.
        assert!(text(&ask(0, "").cells(80)).contains("to unlock elevate in this pane: "));
    }

    #[test]
    fn keys_go_to_the_prompt_only() {
        let mut prompts = Prompts::new(Instant::now());
        prompts.asks.push(ask(1, "/system/commands/id"));
        assert_eq!(prompts.key(0, b"a"), Typed::NotTaken);
        assert_eq!(prompts.key(1, "pé".as_bytes()), Typed::Taken);
        assert_eq!(prompts.key(1, b"\x1b[A"), Typed::Taken);
        assert_eq!(prompts.shown(1).unwrap().typed, "pé".as_bytes());
        // Backspace takes back a whole character.
        prompts.key(1, b"\x7f");
        assert_eq!(prompts.shown(1).unwrap().typed, b"p");
        prompts.key(1, b"\x15");
        assert!(prompts.shown(1).unwrap().typed.is_empty());
        prompts.key(1, b"x");
        assert!(prompts.clear(1));
        assert!(prompts.shown(1).unwrap().typed.is_empty());
        assert!(!prompts.clear(0));
    }

    #[test]
    fn authd_s_lines() {
        let mut prompts = Prompts::new(Instant::now());
        let console_of = |terminal: &str| (terminal == "pts/4").then_some(2);
        let now = Instant::now();
        prompts.take(b"ok\nask 7 pts/4 alice /system/commands/id\n", now, &console_of);
        assert!(prompts.answered);
        let shown = prompts.shown(2).unwrap();
        assert_eq!((shown.id, shown.name.as_str()), (7, "alice"));
        prompts.take(b"ask 8 pts/4 alice \n", now, &console_of);
        // The oldest shows first; done takes one away.
        prompts.take(b"done 7\n", now, &console_of);
        assert_eq!(prompts.shown(2).unwrap().id, 8);
        prompts.take(b"bad line\n", now, &console_of);
        assert_eq!(prompts.asks.len(), 1);
    }
}
