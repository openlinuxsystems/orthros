// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Passwords asked on the consoles, and the Ctrl+Alt+Delete menu
//! (ADR 0251); and bundled's questions about apps' permissions (ADR
//! 0224), which authd passes on the same way: O, A or D answer one, Y or
//! N one from `bundle grant`, and Esc or Ctrl+C close it.
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

use accounts::request::{Answer, Choice, Prompt, Prompted, Question, Request};

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
/// A question with a warning: bright white on red.
const WARNING_FG: u8 = 15;
const WARNING_BG: u8 = 1;

/// A password or a question asked on a console.
pub struct Ask {
    /// authd's number for it.
    id: u64,
    /// The console's index.
    console: usize,
    /// What it asks.
    what: Asking,
}

/// What an [`Ask`] asks.
enum Asking {
    /// `name`'s password, to run `shown`, or to unlock elevate if it is
    /// empty, and what was typed so far.
    Password { name: String, shown: String, typed: Vec<u8> },
    /// A question of bundled's, in its words.
    Choose { question: Question, text: String },
}

impl Drop for Ask {
    fn drop(&mut self) {
        if let Asking::Password { typed, .. } = &mut self.what {
            accounts::wipe(typed);
        }
    }
}

/// `text`'s characters with their widths, control characters shown as
/// `?` and those that take no cell left out.
fn widths(text: &str) -> Vec<(char, usize)> {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .map(|c| (c, term::width(c)))
        .filter(|&(_, width)| width > 0)
        .collect()
}

fn width(chars: &[(char, usize)]) -> usize {
    chars.iter().map(|&(_, w)| w).sum()
}

/// `chars` cut to `room` cells with `...` at the end, if it is wider.
fn cut(chars: &mut Vec<(char, usize)>, room: usize) {
    if width(chars) > room {
        let left = room.saturating_sub(3);
        let mut kept = 0;
        chars.retain(|&(_, w)| {
            kept += w;
            kept <= left
        });
        chars.extend([('.', 1); 3]);
    }
}

/// The cells of `chars` after `cells`, in `blank`'s colours, as many as
/// fit in `cols`.
fn put(cells: &mut Vec<Cell>, chars: &[(char, usize)], blank: Cell, cols: usize) {
    for &(c, width) in chars {
        if cells.len() + width > cols {
            break;
        }
        cells.push(Cell { c, ..blank });
        if width == 2 {
            cells.push(Cell { c: ' ', flags: BOLD | WIDE_TAIL, ..blank });
        }
    }
}

impl Ask {
    /// The bottom line's `cols` cells.
    pub fn cells(&self, cols: usize) -> Vec<Cell> {
        match &self.what {
            Asking::Password { name, shown, .. } => password_cells(cols, name, shown),
            Asking::Choose { question, text } => question_cells(cols, *question, text),
        }
    }

    /// What was typed at a password prompt so far.
    #[cfg(test)]
    fn typed(&self) -> &[u8] {
        match &self.what {
            Asking::Password { typed, .. } => typed,
            Asking::Choose { .. } => &[],
        }
    }
}

/// A question's line: `[permission]` or `[warning]` and its words, cut
/// with `...` if they do not fit, then its keys, which always show.
fn question_cells(cols: usize, question: Question, text: &str) -> Vec<Cell> {
    let (fg, bg, label, keys) = match question {
        Question::Permission => {
            (FG, BG, "[permission] ", "      O: once    A: always    D: don't allow")
        }
        Question::Warning => {
            (WARNING_FG, WARNING_BG, "[warning] ", "      O: once    A: always    D: don't allow")
        }
        Question::Grant => (FG, BG, "[permission] ", "?      Y: yes    N: no"),
    };
    let blank = Cell { c: ' ', fg, bg, flags: BOLD };
    let mut words = widths(label);
    words.extend(widths(text));
    let keys = widths(keys);
    // A space in front.
    cut(&mut words, cols.saturating_sub(1 + width(&keys)));
    let mut cells = vec![blank];
    put(&mut cells, &words, blank, cols);
    put(&mut cells, &keys, blank, cols);
    cells.resize(cols, blank);
    cells.truncate(cols);
    cells
}

/// A password prompt's line: the prompt, the command cut with `...` if it
/// does not fit, `:` and a block where the password goes, which never
/// shows.
fn password_cells(cols: usize, name: &str, shown: &str) -> Vec<Cell> {
    let (lead, command) = {
        let lead = format!("[elevate] Password for {name}, to ");
        if shown.is_empty() {
            (format!("{lead}unlock elevate in this pane"), String::new())
        } else {
            (format!("{lead}run "), shown.to_string())
        }
    };
    let blank = Cell { c: ' ', fg: FG, bg: BG, flags: BOLD };
    let lead = widths(&lead);
    let mut command = widths(&command);
    // A space in front, `: ` and the block after.
    let room = cols.saturating_sub(4);
    cut(&mut command, room.saturating_sub(width(&lead)));
    let mut cells = vec![blank];
    put(&mut cells, &lead, blank, cols);
    put(&mut cells, &command, blank, cols);
    put(&mut cells, &[(':', 1), (' ', 1)], blank, cols);
    if cells.len() < cols {
        cells.push(Cell { flags: BOLD | REVERSE, ..blank });
    }
    cells.resize(cols, blank);
    cells.truncate(cols);
    cells
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
                        let what = Asking::Password { name, shown, typed: Vec::new() };
                        self.asks.push(Ask { id, console, what });
                    }
                    None => answer(id, Prompted::Elsewhere),
                },
                Some(Prompt::Choose { id, terminal, question, text, .. }) => {
                    match console_of(&terminal) {
                        Some(console) => {
                            let what = Asking::Choose { question, text };
                            self.asks.push(Ask { id, console, what });
                        }
                        None => answer(id, Prompted::Elsewhere),
                    }
                }
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
                if let Asking::Password { typed, .. } = &mut ask.what {
                    accounts::wipe(typed);
                    typed.clear();
                }
                true
            }
            None => false,
        }
    }

    /// Bytes a key sent on console `console`: the prompt's there, if one
    /// waits. At a password prompt Enter answers it, Esc or Ctrl+C closes
    /// it, Backspace takes back a character and Ctrl+U all of them; at a
    /// question its letters answer it, and Esc or Ctrl+C close it. Other
    /// keys go nowhere.
    pub fn key(&mut self, console: usize, bytes: &[u8]) -> Typed {
        let Some(at) = self.asks.iter().position(|ask| ask.console == console) else {
            return Typed::NotTaken;
        };
        if let Asking::Choose { question, .. } = self.asks[at].what {
            let chose = match (question, bytes) {
                (_, b"\x1b" | b"\x03") => Some(Prompted::Cancelled),
                (Question::Grant, b"y" | b"Y") => Some(Prompted::Chose(Choice::Always)),
                (Question::Grant, b"n" | b"N") => Some(Prompted::Chose(Choice::Never)),
                (Question::Grant, _) => None,
                (_, b"o" | b"O") => Some(Prompted::Chose(Choice::Once)),
                (_, b"a" | b"A") => Some(Prompted::Chose(Choice::Always)),
                (_, b"d" | b"D") => Some(Prompted::Chose(Choice::Never)),
                _ => None,
            };
            if let Some(chose) = chose {
                let ask = self.asks.remove(at);
                answer(ask.id, chose);
            }
            return Typed::Taken;
        }
        let id = self.asks[at].id;
        let Asking::Password { typed, .. } = &mut self.asks[at].what else { return Typed::Taken };
        match bytes {
            b"\r" | b"\n" => {
                let password = Prompted::Password(typed.clone());
                self.asks.remove(at);
                answer(id, password);
            }
            b"\x1b" | b"\x03" => {
                self.asks.remove(at);
                answer(id, Prompted::Cancelled);
            }
            b"\x7f" | b"\x08" => {
                // A whole character, however many bytes it took.
                while let Some(byte) = typed.pop() {
                    if byte & 0xc0 != 0x80 {
                        break;
                    }
                }
            }
            b"\x15" => {
                accounts::wipe(typed);
                typed.clear();
            }
            [0x1b, ..] => {}
            // Text, as much as authd takes.
            text if text.iter().all(|&b| b >= 0x20 && b != 0x7f)
                && typed.len() + text.len() <= TYPED_MAX =>
            {
                typed.extend_from_slice(text);
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
        let what =
            Asking::Password { name: "alice".into(), shown: shown.into(), typed: Vec::new() };
        Ask { id: 3, console, what }
    }

    fn question(console: usize, question: Question, text: &str) -> Ask {
        Ask { id: 4, console, what: Asking::Choose { question, text: text.into() } }
    }

    #[test]
    fn a_question_fits_the_line_with_its_keys() {
        let words = "Notes (org.example.notes) asks to: use your documents folder, to read and \
                     change it";
        let cells = question(0, Question::Permission, words).cells(160);
        let shown = text(&cells);
        assert!(shown.starts_with(&format!(" [permission] {words}      O: once")), "{shown}");
        assert!(shown.trim_end().ends_with("O: once    A: always    D: don't allow"), "{shown}");
        assert_eq!((cells[1].fg, cells[1].bg), (FG, BG));
        // At 80 columns the words are cut and the keys stay.
        let cut = text(&question(0, Question::Permission, words).cells(80));
        assert_eq!(cut.chars().count(), 80);
        assert!(cut.contains("...      O: once    A: always    D: don't allow"), "{cut}");
        // A warning is white on red; bundle grant's keys are Y and N.
        let warning = question(0, Question::Warning, "Backup asks to: use all of your files");
        let cells = warning.cells(80);
        assert!(text(&cells).starts_with(" [warning] Backup asks to"));
        assert_eq!((cells[1].fg, cells[1].bg), (WARNING_FG, WARNING_BG));
        let grant = text(&question(0, Question::Grant, "Give Notes: x").cells(80));
        assert!(grant.contains("Give Notes: x?      Y: yes    N: no"), "{grant}");
    }

    #[test]
    fn a_question_takes_only_its_keys() {
        let mut prompts = Prompts::new(Instant::now());
        prompts.asks.push(question(1, Question::Grant, "Give Notes: x"));
        // Keys that answer nothing stay with the prompt, which stays.
        for key in [&b"o"[..], b"a", b"d", b"\r", b"x", b"\x1b[A"] {
            assert_eq!(prompts.key(1, key), Typed::Taken);
            assert!(prompts.shown(1).is_some());
        }
        assert!(prompts.clear(1));
        assert_eq!(prompts.key(0, b"y"), Typed::NotTaken);
        // Esc closes it (its answer goes to authd, which is not there).
        assert_eq!(prompts.key(1, b"\x1b"), Typed::Taken);
        assert!(prompts.shown(1).is_none());
        prompts.asks.push(question(2, Question::Permission, "x"));
        assert_eq!(prompts.key(2, b"y"), Typed::Taken);
        assert!(prompts.shown(2).is_some());
        assert_eq!(prompts.key(2, b"A"), Typed::Taken);
        assert!(prompts.shown(2).is_none());
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
        assert_eq!(prompts.shown(1).unwrap().typed(), "pé".as_bytes());
        // Backspace takes back a whole character.
        prompts.key(1, b"\x7f");
        assert_eq!(prompts.shown(1).unwrap().typed(), b"p");
        prompts.key(1, b"\x15");
        assert!(prompts.shown(1).unwrap().typed().is_empty());
        prompts.key(1, b"x");
        assert!(prompts.clear(1));
        assert!(prompts.shown(1).unwrap().typed().is_empty());
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
        assert_eq!(shown.id, 7);
        assert!(matches!(&shown.what, Asking::Password { name, .. } if name == "alice"));
        prompts.take(b"ask 8 pts/4 alice \n", now, &console_of);
        // The oldest shows first; done takes one away.
        prompts.take(b"done 7\n", now, &console_of);
        assert_eq!(prompts.shown(2).unwrap().id, 8);
        prompts.take(b"bad line\n", now, &console_of);
        assert_eq!(prompts.asks.len(), 1);
        prompts.take(b"choose 9 pts/4 alice warning Backup asks to: x\n", now, &console_of);
        assert_eq!(prompts.asks.len(), 2);
        let last = &prompts.asks[1];
        assert!(matches!(&last.what, Asking::Choose { question: Question::Warning, text }
            if text == "Backup asks to: x"));
    }
}
