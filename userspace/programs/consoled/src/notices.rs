// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Notifications on the screen's bottom line (ADR 0212).
//!
//! consoled follows notifyd(8)'s notifications and shows the newest on
//! the bottom line of the screen, over the shown console's last line, for
//! as long as it asks (8 seconds unless it says otherwise), or a critical
//! one, or one of 0 seconds, until a key is pressed. A newer notification
//! takes the line at once; `notifications` lists them all. While notifyd
//! is not running, consoled tries again every 5 seconds.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::term::{self, BOLD, Cell, WIDE_TAIL};

/// notifyd's socket.
pub const SOCKET: &str = "/system/live/notify/socket";

/// How long consoled waits to try notifyd again.
const RETRY: Duration = Duration::from_secs(5);

/// notifyd's request for new notifications as they come, none of the old.
const FOLLOW: &[u8] = b"{\"ask\":\"list\",\"count\":0,\"follow\":true}\n";

/// The most bytes of a line from notifyd; a longer one is dropped.
const LINE_MAX: usize = 64 * 1024;

/// A non-blocking connection to the socket at `path`, made at once or
/// not at all: consoled's loop must never wait, not even for a notifyd
/// or an authd that does not accept.
pub fn connect_now(path: &str) -> io::Result<UnixStream> {
    // SAFETY: socket(2) takes no pointers.
    let fd = unsafe {
        libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0)
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a new socket, owned by nothing else.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    // SAFETY: all zeros is a valid sockaddr_un.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    if path.len() >= address.sun_path.len() {
        return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG));
    }
    for (to, &from) in address.sun_path.iter_mut().zip(path.as_bytes()) {
        *to = from as libc::c_char;
    }
    let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
    // SAFETY: `address` is a valid sockaddr_un of `len` bytes.
    let done = unsafe { libc::connect(fd, (&raw const address).cast(), len) };
    if done != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stream)
}

/// What consoled reads of a notification (notifyd(8)).
#[derive(Debug, Deserialize)]
struct Notification {
    name: String,
    #[serde(default)]
    app: Option<String>,
    #[serde(default)]
    urgency: String,
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    seconds: u32,
    #[serde(default)]
    terminal: Option<String>,
}

/// A notification on the bottom line.
#[derive(Debug, PartialEq, Eq)]
pub struct Shown {
    /// The line's text: who sent it, its title and its body.
    pub text: String,
    pub urgency: Urgency,
    /// When it goes; None for when a key is pressed.
    pub until: Option<Instant>,
    /// The terminal it is about, as `pts/3`: it is not shown while that
    /// terminal's console is (ADR 0263), as the person sees it already.
    pub terminal: Option<String>,
}

/// How a notification looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Urgency {
    Low,
    Normal,
    Critical,
}

impl Shown {
    fn of(notification: &Notification, now: Instant) -> Shown {
        let from = match &notification.app {
            Some(app) if *app != notification.name => format!("{} ({app})", notification.name),
            _ => notification.name.clone(),
        };
        let mut text = format!("{from}: {}", notification.title);
        let body: Vec<&str> =
            notification.body.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        if !body.is_empty() {
            text.push_str(" - ");
            text.push_str(&body.join(" "));
        }
        let urgency = match notification.urgency.as_str() {
            "low" => Urgency::Low,
            "critical" => Urgency::Critical,
            _ => Urgency::Normal,
        };
        let until = (urgency != Urgency::Critical && notification.seconds > 0)
            .then(|| now + Duration::from_secs(u64::from(notification.seconds)));
        Shown { text, urgency, until, terminal: notification.terminal.clone() }
    }

    /// The bottom line's `cols` cells: the text after a space, in white on
    /// blue, or on red and bold for a critical one, or in black on grey for
    /// a low one; cut with `...` if it does not fit.
    pub fn cells(&self, cols: usize) -> Vec<Cell> {
        let (fg, bg, flags) = match self.urgency {
            Urgency::Low => (0, 7, 0),
            Urgency::Normal => (15, 4, 0),
            Urgency::Critical => (15, 1, BOLD),
        };
        let blank = Cell { c: ' ', fg, bg, flags };
        // Control characters never reach the screen as themselves.
        let chars = self.text.chars().map(|c| if c.is_control() { '?' } else { c });
        let shown: Vec<(char, usize)> =
            chars.map(|c| (c, term::width(c))).filter(|&(_, width)| width > 0).collect();
        // After the space in front.
        let fits = shown.iter().map(|&(_, width)| width).sum::<usize>() < cols;
        let room = if fits { cols } else { cols.saturating_sub(3) };
        let mut cells = vec![blank];
        for (c, width) in shown {
            if cells.len() + width > room {
                break;
            }
            cells.push(Cell { c, ..blank });
            if width == 2 {
                cells.push(Cell { c: ' ', flags: flags | WIDE_TAIL, ..blank });
            }
        }
        if !fits {
            cells.resize(room, blank);
            cells.extend([Cell { c: '.', ..blank }; 3]);
        }
        cells.resize(cols, blank);
        cells.truncate(cols);
        cells
    }
}

/// consoled's connection to notifyd and what it shows.
pub struct Notices {
    stream: Option<UnixStream>,
    /// A line read in part.
    pending: Vec<u8>,
    /// When to try notifyd again.
    retry_at: Option<Instant>,
    /// Whether consoled said notifyd went away, so it says so once.
    said_gone: bool,
    /// The notification on the bottom line.
    pub shown: Option<Shown>,
}

impl Notices {
    /// No connection yet: the first try is at once.
    pub fn new(now: Instant) -> Notices {
        Notices {
            stream: None,
            pending: Vec::new(),
            retry_at: Some(now),
            said_gone: false,
            shown: None,
        }
    }

    /// The connection's descriptor, to poll; -1 for none.
    pub fn fd(&self) -> i32 {
        self.stream.as_ref().map_or(-1, AsRawFd::as_raw_fd)
    }

    /// When something is to be done next: trying notifyd again, or taking
    /// a notification off the line.
    pub fn deadline(&self) -> Option<Instant> {
        let until = self.shown.as_ref().and_then(|shown| shown.until);
        match (self.retry_at, until) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Does what is due at `now`: tries notifyd again, or takes a
    /// notification whose time is up off the line.
    pub fn tick(&mut self, now: Instant) {
        if self.retry_at.is_some_and(|at| at <= now) {
            self.connect(now);
        }
        if self.shown.as_ref().and_then(|shown| shown.until).is_some_and(|until| until <= now) {
            self.shown = None;
        }
    }

    fn connect(&mut self, now: Instant) {
        let connected = connect_now(SOCKET).and_then(|mut stream| {
            stream.write_all(FOLLOW)?;
            Ok(stream)
        });
        match connected {
            Ok(stream) => {
                eprintln!("consoled: showing notifications from notifyd");
                self.stream = Some(stream);
                self.retry_at = None;
                self.said_gone = false;
            }
            Err(_) => self.retry_at = Some(now + RETRY),
        }
    }

    fn lost(&mut self, now: Instant, why: &str) {
        if !self.said_gone {
            eprintln!("consoled: lost notifyd ({why}); trying again every 5 seconds");
            self.said_gone = true;
        }
        self.stream = None;
        self.pending.clear();
        self.retry_at = Some(now + RETRY);
    }

    /// Reads what notifyd sent.
    pub fn read(&mut self, now: Instant) {
        let mut buffer = [0u8; 4096];
        loop {
            let Some(stream) = self.stream.as_mut() else { return };
            match stream.read(&mut buffer) {
                Ok(0) => return self.lost(now, "it closed the connection"),
                Ok(n) => self.take(&buffer[..n], now),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return,
                Err(err) => return self.lost(now, &err.to_string()),
            }
        }
    }

    /// Takes in `bytes` from notifyd, and shows the last whole
    /// notification in them.
    fn take(&mut self, bytes: &[u8], now: Instant) {
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            if let Ok(notification) = serde_json::from_slice::<Notification>(&line) {
                self.shown = Some(Shown::of(&notification, now));
            }
        }
        if self.pending.len() > LINE_MAX {
            self.pending.clear();
        }
    }

    /// A key was pressed: it takes a notification that waits for one off
    /// the line.
    pub fn key(&mut self) {
        if self.shown.as_ref().is_some_and(|shown| shown.until.is_none()) {
            self.shown = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(cells: &[Cell]) -> String {
        cells.iter().filter(|cell| cell.flags & WIDE_TAIL == 0).map(|cell| cell.c).collect()
    }

    #[test]
    fn shows_the_newest() {
        let now = Instant::now();
        let mut notices = Notices::new(now);
        let first = br#"{"id":1,"time":"t","process":57,"name":"notify-send","app":"backup","urgency":"normal","title":"Backup done","body":"3 disks\ncopied","seconds":8}"#;
        notices.take(first, now);
        assert!(notices.shown.is_none(), "not a whole line yet");
        notices.take(b"\n", now);
        let shown = notices.shown.as_ref().unwrap();
        assert_eq!(shown.text, "notify-send (backup): Backup done - 3 disks copied");
        assert_eq!(shown.until, Some(now + Duration::from_secs(8)));
        // A key does not take a timed one off; its time does.
        notices.retry_at = None;
        notices.key();
        notices.tick(now + Duration::from_secs(7));
        assert!(notices.shown.is_some());
        notices.tick(now + Duration::from_secs(8));
        assert!(notices.shown.is_none());
        // A critical one stays until a key is pressed; a bad line is skipped.
        let critical = b"not json\n{\"name\":\"powerd\",\"urgency\":\"critical\",\"title\":\"Battery low\",\"seconds\":0}\n";
        notices.take(critical, now);
        assert_eq!(notices.shown.as_ref().unwrap().until, None);
        notices.tick(now + Duration::from_secs(3600));
        assert!(notices.shown.is_some());
        notices.key();
        assert!(notices.shown.is_none());
    }

    #[test]
    fn fits_the_line() {
        let shown = Shown { text: "a: b".into(), urgency: Urgency::Normal, until: None, terminal: None };
        let cells = shown.cells(10);
        assert_eq!(text(&cells), " a: b     ");
        assert_eq!((cells[0].fg, cells[0].bg), (15, 4));
        let shown = Shown { text: "x: 0123456789".into(), urgency: Urgency::Critical, until: None, terminal: None };
        let cells = shown.cells(10);
        assert_eq!(text(&cells), " x: 012...");
        assert_eq!(cells.len(), 10);
        assert_eq!((cells[1].bg, cells[1].flags), (1, BOLD));
        // Exactly full: no dots.
        let shown = Shown { text: "x: 012345".into(), urgency: Urgency::Low, until: None, terminal: None };
        assert_eq!(text(&shown.cells(10)), " x: 012345");
        // Wide characters take two cells; control characters show as `?`.
        let shown = Shown { text: "日本\u{1b}[2J".into(), urgency: Urgency::Normal, until: None, terminal: None };
        let cells = shown.cells(12);
        assert_eq!(text(&cells), " 日本?[2J   ");
        assert_eq!(cells[2].flags & WIDE_TAIL, WIDE_TAIL);
        // A wide character that would not fit before the dots goes.
        let shown =
            Shown { text: "ab日本語です".into(), urgency: Urgency::Normal, until: None, terminal: None };
        let cells = shown.cells(8);
        assert_eq!((cells.len(), text(&cells).as_str()), (8, " ab日..."));
    }
}
