// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! notifyd(8): the notifications service (ADR 0212).
//!
//! A program sends a notification to notifyd's socket as a line of JSON:
//! a title, a body, an urgency and how long to show it. notifyd puts on
//! it the number and name of the process that sent it, as the kernel
//! gives them (`socket_state`), keeps the last 64 in memory and sends
//! each to the programs that follow them: consoled(8) shows it on the
//! console's bottom line. `notify-send` sends one, and `notifications`
//! lists or follows them. Nothing of a notification is written to the
//! disk or to the log.
//!
//! notifyd answers on one thread: each connection sends one request and
//! has until [`REQUEST_TIME`] to send it, and an answer that the other end
//! does not read in time ends its connection, so no program can hold
//! notifyd up.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::args::{Arg, Args};
use crate::logd::{self, Refusal};
use crate::util::{self, Fail, fail};
use crate::{proc, sys};

/// The socket notifyd answers on.
pub const SOCKET: &str = "/system/live/notify/socket";

/// The notifications notifyd keeps, for `notifications`.
pub const KEPT: usize = 64;

/// The most connections at a time, and from one process; more wait to
/// be accepted, or are refused.
const CLIENTS_MAX: usize = 64;
const CLIENTS_PER_PROCESS: usize = 8;

/// The most characters of a title and of a body; the rest is cut.
pub const TITLE_MAX: usize = 120;
pub const BODY_MAX: usize = 1000;

/// How long a notification is shown, unless it asks for another time, and
/// the longest it may ask for, in seconds.
pub const SECONDS_DEFAULT: u32 = 8;
pub const SECONDS_MAX: u32 = 60;

/// A process may send [`BURST`] notifications in any [`WINDOW`]; notifyd
/// refuses more.
pub const BURST: usize = 10;
pub const WINDOW: Duration = Duration::from_secs(10);

/// How long a connection has to send its request.
const REQUEST_TIME: Duration = Duration::from_secs(5);

/// The longest request line.
const REQUEST_MAX: usize = 16 * 1024;

/// The most bytes of answers waiting for a follower to read them; one
/// that falls further behind is cut off.
const BEHIND_MAX: usize = 256 * 1024;

/// How urgent a notification is, as the freedesktop.org notification
/// specification's urgency levels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Urgency {
    Low,
    #[default]
    Normal,
    /// Shown until a key is pressed.
    Critical,
}

impl Urgency {
    /// The urgency named `name`.
    pub fn parse(name: &str) -> Option<Urgency> {
        match name {
            "low" => Some(Urgency::Low),
            "normal" => Some(Urgency::Normal),
            "critical" => Some(Urgency::Critical),
            _ => None,
        }
    }
}

/// A request to notifyd: a line of JSON.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Request {
    /// `send` or `list`.
    pub ask: String,
    /// send: the notification's title, its first line.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// send: the text under the title, which may hold newlines.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// send: the name the program goes by, if not its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// send: how urgent it is; normal if not given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub urgency: Option<Urgency>,
    /// send: how many seconds to show it, 0 for until a key is pressed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seconds: Option<u32>,
    /// send: the terminal the sender runs on, as `pts/3`: consoled does
    /// not show the notification while that terminal's console is the one
    /// shown (ADR 0263).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<String>,
    /// list: the newest this many of those kept; all if not given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
    /// list: then go on sending each new one as it comes.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub follow: bool,
}

/// A notification as notifyd keeps it and sends it to followers: a line
/// of JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    /// Its number, counting from 1 since notifyd started.
    pub id: u64,
    /// When notifyd took it, RFC 3339 in UTC.
    pub time: String,
    /// The process that sent it, and that process's name, as the kernel
    /// gives them.
    pub process: u64,
    pub name: String,
    /// The name the program said it goes by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    pub urgency: Urgency,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// How long to show it in seconds; 0 for until a key is pressed.
    pub seconds: u32,
    /// The terminal the sender said it runs on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<String>,
}

impl Notification {
    /// Who sent it, for showing: the program's name, then the name it
    /// said it goes by if that is another.
    pub fn from(&self) -> String {
        match &self.app {
            Some(app) if *app != self.name => format!("{} ({app})", self.name),
            _ => self.name.clone(),
        }
    }
}

/// notifyd's answer to a notification sent.
#[derive(Debug, Serialize, Deserialize)]
pub struct Sent {
    pub id: u64,
}

/// `text` as notifyd keeps it: without spaces at either end, a tab as a
/// space, a newline kept if `newlines` (or a space), a carriage return
/// dropped and any other control character as `?`, and at most `max`
/// characters, the last three `...` if it was cut.
pub fn clean(text: &str, max: usize, newlines: bool) -> String {
    let mut out: Vec<char> = text
        .trim()
        .chars()
        .filter(|&c| c != '\r')
        .map(|c| match c {
            '\n' if newlines => '\n',
            '\t' | '\n' => ' ',
            c if c.is_control() => '?',
            c => c,
        })
        .collect();
    if out.len() > max {
        out.truncate(max.saturating_sub(3));
        out.extend("...".chars());
    }
    out.into_iter().collect()
}

/// The notification `request` asks to send, numbered `id` and taken at
/// `time`, from process `process` named `name`; or why it is refused.
pub fn notification(
    request: &Request,
    id: u64,
    time: String,
    process: u64,
    name: &str,
) -> Result<Notification, String> {
    let title = clean(&request.title, TITLE_MAX, false);
    if title.is_empty() {
        return Err("a notification needs a title".into());
    }
    let urgency = request.urgency.unwrap_or_default();
    let seconds = match (urgency, request.seconds) {
        (Urgency::Critical, _) => 0,
        (_, None) => SECONDS_DEFAULT,
        (_, Some(seconds)) => seconds.min(SECONDS_MAX),
    };
    let app = request.app.as_deref().map(|app| clean(app, 64, false)).filter(|a| !a.is_empty());
    Ok(Notification {
        id,
        time,
        process,
        name: clean(name, 64, false),
        app,
        urgency,
        title,
        body: clean(&request.body, BODY_MAX, true),
        seconds,
        terminal: request
            .terminal
            .as_deref()
            .map(|t| clean(t, 32, false))
            .filter(|t| !t.is_empty()),
    })
}

/// How many notifications each process sent lately, to refuse a flood.
#[derive(Default)]
pub struct Limiter {
    sent: HashMap<u64, VecDeque<Instant>>,
}

impl Limiter {
    /// Whether process `process` may send one more at `now`; counts it if
    /// so. Forgets the processes that sent none for a [`WINDOW`].
    pub fn allow(&mut self, process: u64, now: Instant) -> bool {
        let recent = |at: &Instant| now.saturating_duration_since(*at) < WINDOW;
        self.sent.retain(|_, times| times.back().is_some_and(recent));
        let times = self.sent.entry(process).or_default();
        while times.front().is_some_and(|at| !recent(at)) {
            times.pop_front();
        }
        if times.len() >= BURST {
            return false;
        }
        times.push_back(now);
        true
    }
}

/// One connection.
struct Client {
    stream: UnixStream,
    /// The process at the other end, and its name, when it connected.
    process: u64,
    name: String,
    /// The request read so far.
    request: Vec<u8>,
    /// When it must have sent its request, or read its answer.
    deadline: Instant,
    /// What is still to be written to it.
    out: Vec<u8>,
    /// It follows the notifications.
    following: bool,
    /// Once `out` is written, the connection ends.
    closing: bool,
    /// It is to be dropped.
    done: bool,
}

impl Client {
    /// Queues `line` to be written; the other end has until
    /// [`REQUEST_TIME`] from `now` to start reading it.
    fn queue(&mut self, line: &impl Serialize, now: Instant) {
        if self.out.is_empty() {
            self.deadline = now + REQUEST_TIME;
        }
        if serde_json::to_writer(&mut self.out, line).is_ok() {
            self.out.push(b'\n');
        }
    }

    /// Writes what it can of `out`; ends the connection on an error, or
    /// once all is written if it is closing.
    fn flush(&mut self) {
        while !self.out.is_empty() {
            match self.stream.write(&self.out) {
                Ok(0) => break,
                Ok(n) => {
                    self.out.drain(..n);
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.done = true;
                    return;
                }
            }
        }
        if self.out.is_empty() && self.closing {
            self.done = true;
        }
        if self.out.len() > BEHIND_MAX {
            self.done = true;
        }
    }
}

/// notifyd's state.
struct Notifyd {
    listener: UnixListener,
    clients: Vec<Client>,
    kept: VecDeque<Notification>,
    next: u64,
    limiter: Limiter,
    /// The processes refused lately, so each flood is logged once.
    refused: HashMap<u64, Instant>,
}

/// Set by SIGTERM: notifyd ends.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// The time now, as RFC 3339 in UTC.
fn now_rfc3339() -> String {
    let ns = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    logd::rfc3339(u64::try_from(ns).unwrap_or(u64::MAX))
}

impl Notifyd {
    fn accept(&mut self, now: Instant) {
        while self.clients.len() < CLIENTS_MAX {
            let stream = match self.listener.accept() {
                Ok((stream, _)) => stream,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            };
            if stream.set_nonblocking(true).is_err() {
                continue;
            }
            let slot = sys::slot_of(stream.as_raw_fd());
            let process = radix_abi::file::socket_state(&sys::SYS, slot).map_or(0, |s| s.peer);
            let name = proc::process(process).map_or_else(|_| "unknown".into(), |p| p.name);
            let held = self.clients.iter().filter(|c| c.process == process).count();
            self.clients.push(Client {
                stream,
                process,
                name: clean(&name, 64, false),
                request: Vec::new(),
                deadline: now + REQUEST_TIME,
                out: Vec::new(),
                following: false,
                closing: false,
                done: false,
            });
            if held >= CLIENTS_PER_PROCESS {
                let error = format!("more than {CLIENTS_PER_PROCESS} connections from one program");
                self.refuse(self.clients.len() - 1, error, now);
            }
        }
    }

    /// Reads what client `i` sent, and answers its request once it is
    /// whole. Reads at most 64 KiB at a time, so that one connection
    /// that keeps sending does not keep notifyd from the others.
    fn read(&mut self, i: usize, now: Instant) {
        let mut buffer = [0u8; 4096];
        for _ in 0..16 {
            let client = &mut self.clients[i];
            match client.stream.read(&mut buffer) {
                Ok(0) => {
                    client.done = true;
                    return;
                }
                // A follower's bytes mean nothing; they are dropped.
                Ok(_) if client.following || client.closing => {}
                Ok(n) => {
                    client.request.extend_from_slice(&buffer[..n]);
                    if let Some(end) = client.request.iter().position(|&b| b == b'\n') {
                        let line = client.request[..end].to_vec();
                        client.request.clear();
                        self.answer(i, &line, now);
                    } else if client.request.len() > REQUEST_MAX {
                        self.refuse(i, "the request is too long".into(), now);
                        return;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => {
                    self.clients[i].done = true;
                    return;
                }
            }
        }
    }

    fn refuse(&mut self, i: usize, error: String, now: Instant) {
        let client = &mut self.clients[i];
        client.queue(&Refusal { error }, now);
        client.closing = true;
        client.flush();
    }

    /// Answers request `line` of client `i`.
    fn answer(&mut self, i: usize, line: &[u8], now: Instant) {
        let request: Request = match serde_json::from_slice(line) {
            Ok(request) => request,
            Err(err) => return self.refuse(i, format!("not a request: {err}"), now),
        };
        match request.ask.as_str() {
            "send" => self.send(i, &request, now),
            "list" => {
                let client = &mut self.clients[i];
                let skip = request.count.map_or(0, |n| self.kept.len().saturating_sub(n));
                for notification in self.kept.iter().skip(skip) {
                    client.queue(notification, now);
                }
                // A follower waits as long as it likes, but must keep up.
                client.following = request.follow;
                client.closing = !request.follow;
                client.flush();
            }
            other => self.refuse(i, format!("notifyd does not know '{other}'"), now),
        }
    }

    fn send(&mut self, i: usize, request: &Request, now: Instant) {
        let (process, name) = (self.clients[i].process, self.clients[i].name.clone());
        if !self.limiter.allow(process, now) {
            let said = self.refused.get(&process).is_some_and(|at| now - *at < WINDOW);
            if !said {
                eprintln!(
                    "<4>notifyd: refusing notifications from {name} (process {process}) for a \
                     while: more than {BURST} in {} seconds",
                    WINDOW.as_secs()
                );
                self.refused.insert(process, now);
            }
            self.refused.retain(|_, at| now - *at < WINDOW);
            let error = "too many notifications; try again in a few seconds";
            return self.refuse(i, error.into(), now);
        }
        let notification = match notification(request, self.next, now_rfc3339(), process, &name) {
            Ok(notification) => notification,
            Err(error) => return self.refuse(i, error, now),
        };
        self.next += 1;
        let client = &mut self.clients[i];
        client.queue(&Sent { id: notification.id }, now);
        client.closing = true;
        client.flush();
        for client in self.clients.iter_mut().filter(|c| c.following && !c.done) {
            client.queue(&notification, now);
            client.flush();
        }
        if self.kept.len() == KEPT {
            self.kept.pop_front();
        }
        self.kept.push_back(notification);
    }

    fn run(&mut self) -> io::Result<()> {
        loop {
            if STOP.load(Ordering::Relaxed) {
                return Ok(());
            }
            let now = Instant::now();
            let listening = self.clients.len() < CLIENTS_MAX;
            let mut fds = vec![libc::pollfd {
                fd: if listening { self.listener.as_raw_fd() } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            }];
            for client in &self.clients {
                let out = if client.out.is_empty() { 0 } else { libc::POLLOUT };
                fds.push(libc::pollfd {
                    fd: client.stream.as_raw_fd(),
                    events: libc::POLLIN | out,
                    revents: 0,
                });
            }
            // The first deadline: a request not yet sent, or an answer
            // not yet read.
            let deadline = self
                .clients
                .iter()
                .filter(|c| !c.following || !c.out.is_empty())
                .map(|c| c.deadline)
                .min();
            let timeout = deadline.map_or(-1, |at| {
                at.saturating_duration_since(now).as_millis().min(60_000) as libc::c_int + 1
            });
            // SAFETY: `fds` holds valid pollfds.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
                continue;
            }
            let now = Instant::now();
            // The clients that were there when poll was called, in order;
            // accepted ones come after them.
            for (i, entry) in fds[1..].iter().enumerate() {
                if entry.revents & libc::POLLOUT != 0 {
                    let client = &mut self.clients[i];
                    let before = client.out.len();
                    client.flush();
                    if client.out.len() < before {
                        client.deadline = now + REQUEST_TIME;
                    }
                }
                if entry.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                    self.read(i, now);
                }
            }
            for client in &mut self.clients {
                let waiting = !client.following || !client.out.is_empty();
                if waiting && client.deadline <= now {
                    client.done = true;
                }
            }
            self.clients.retain(|c| !c.done);
            if fds[0].revents != 0 {
                self.accept(now);
            }
        }
    }
}

/// notifyd(8).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    if let Some(arg) = args.next()? {
        return Err(args.unknown(&arg));
    }
    // SAFETY: setting a signal's disposition touches no memory; no other
    // thread runs. A follower that goes away must not end notifyd: the
    // write to it fails instead.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    // SAFETY: all zeros is a valid sigaction: numbers, a signal set and a
    // function address.
    let mut action: libc::sigaction = unsafe { util::zeroed() };
    action.sa_sigaction = on_term as extern "C" fn(libc::c_int) as *const () as usize;
    // SAFETY: the handler only stores to an atomic.
    unsafe { libc::sigaction(libc::SIGTERM, &raw const action, std::ptr::null_mut()) };
    let dir = Path::new(SOCKET).parent().unwrap_or(Path::new("/"));
    std::fs::create_dir_all(dir).map_err(|e| util::failed(&dir.display(), e))?;
    let _ = std::fs::remove_file(SOCKET);
    let listener = UnixListener::bind(SOCKET).map_err(|e| util::failed(&SOCKET, e))?;
    // Every program may send, whoever runs it (the limiter keeps any one
    // from flooding): the folder and the socket are open to all, as
    // authd's is.
    {
        use std::os::unix::fs::PermissionsExt;
        let open = |path: &Path, mode| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                .map_err(|e| util::failed(&path.display(), e))
        };
        open(dir, 0o755)?;
        open(Path::new(SOCKET), 0o666)?;
    }
    listener.set_nonblocking(true).map_err(|e| util::failed(&SOCKET, e))?;
    eprintln!("notifyd: taking notifications at {SOCKET}");
    let mut notifyd = Notifyd {
        listener,
        clients: Vec::new(),
        kept: VecDeque::with_capacity(KEPT),
        next: 1,
        limiter: Limiter::default(),
        refused: HashMap::new(),
    };
    notifyd.run().map_err(|e| util::failed(&"notifyd", e))?;
    let _ = std::fs::remove_file(SOCKET);
    Ok(0)
}

/// Sends `request` to notifyd and calls `each` with each line of the
/// answer until notifyd closes the connection or `each` says to stop.
pub fn ask(
    request: &Request,
    mut each: impl FnMut(&str) -> Result<bool, Fail>,
) -> Result<(), Fail> {
    let mut stream = UnixStream::connect(SOCKET).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
            fail(format!("notifyd is not running ({SOCKET}: {})", util::strip_os_error(&e)))
        }
        _ => util::failed(&SOCKET, e),
    })?;
    let mut line = serde_json::to_string(request).map_err(|e| fail(e.to_string()))?;
    line.push('\n');
    stream.write_all(line.as_bytes()).map_err(|e| util::failed(&SOCKET, e))?;
    for line in io::BufRead::lines(io::BufReader::new(stream)) {
        let line = line.map_err(|e| util::failed(&SOCKET, e))?;
        if let Ok(refusal) = serde_json::from_str::<Refusal>(&line) {
            return Err(fail(refusal.error));
        }
        if !each(&line)? {
            break;
        }
    }
    Ok(())
}

/// notify-send(1).
pub fn notify_send(args: &mut Args) -> Result<i32, Fail> {
    let mut request = Request { ask: "send".into(), ..Request::default() };
    let mut print_id = false;
    let mut operands = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('u') => request.urgency = Some(urgency(&args.text()?)?),
            Arg::Long(l) if l == "urgency" => request.urgency = Some(urgency(&args.text()?)?),
            Arg::Short('t') => request.seconds = Some(expire_time(&args.text()?)?),
            Arg::Long(l) if l == "expire-time" => {
                request.seconds = Some(expire_time(&args.text()?)?);
            }
            Arg::Short('a') => request.app = Some(args.text()?),
            Arg::Long(l) if l == "app-name" => request.app = Some(args.text()?),
            Arg::Short('p') => print_id = true,
            Arg::Long(l) if l == "print-id" => print_id = true,
            // An icon, a category and hints: the console shows none.
            Arg::Short('i' | 'c' | 'h') => {
                args.value()?;
            }
            Arg::Long(l) if l == "icon" || l == "category" || l == "hint" => {
                args.value()?;
            }
            Arg::Short('e') => {}
            Arg::Long(l) if l == "transient" => {}
            Arg::Operand(operand) => operands.push(operand.to_string_lossy().into_owned()),
            other => return Err(args.unknown(&other)),
        }
    }
    let mut operands = operands.into_iter();
    request.title = operands.next().ok_or_else(|| util::usage("no title given".into()))?;
    request.body = operands.next().unwrap_or_default();
    if operands.next().is_some() {
        return Err(util::usage("too many operands: a title and a body at most".into()));
    }
    let mut sent = None;
    ask(&request, |line| {
        sent = serde_json::from_str::<Sent>(line).ok();
        Ok(false)
    })?;
    let sent = sent.ok_or_else(|| fail("notifyd did not take the notification".into()))?;
    if print_id {
        println!("{}", sent.id);
    }
    Ok(0)
}

fn urgency(text: &str) -> Result<Urgency, Fail> {
    Urgency::parse(text)
        .ok_or_else(|| util::usage(format!("'{text}' is not an urgency: low, normal or critical")))
}

/// The seconds of `--expire-time`'s milliseconds, rounded up; 0 for
/// until a key is pressed, as libnotify's 0 is for never.
fn expire_time(text: &str) -> Result<u32, Fail> {
    let ms: u64 =
        text.parse().map_err(|_| util::usage(format!("'{text}' is not a time in milliseconds")))?;
    Ok(u32::try_from(ms.div_ceil(1000)).unwrap_or(u32::MAX).min(SECONDS_MAX))
}

/// notifications(1).
pub fn notifications(args: &mut Args) -> Result<i32, Fail> {
    let mut request = Request { ask: "list".into(), ..Request::default() };
    let mut json = false;
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('f') => request.follow = true,
            Arg::Long(l) if l == "follow" => request.follow = true,
            Arg::Short('n') => request.count = Some(args.number()?),
            Arg::Long(l) if l == "lines" => request.count = Some(args.number()?),
            Arg::Long(l) if l == "json" => json = true,
            other => return Err(args.unknown(&other)),
        }
    }
    let mut out = io::stdout().lock();
    ask(&request, |line| {
        let written = if json {
            writeln!(out, "{line}")
        } else {
            match serde_json::from_str::<Notification>(line) {
                Ok(notification) => out.write_all(show(&notification).as_bytes()),
                Err(_) => Ok(()),
            }
        };
        // Each as it comes, for a follower.
        Ok(written.and_then(|()| out.flush()).is_ok())
    })?;
    Ok(0)
}

/// `notification` as `notifications` shows it: when, who, and the title,
/// then the body's lines indented.
pub fn show(notification: &Notification) -> String {
    let when = notification.time.get(..19).unwrap_or(&notification.time).replace('T', " ");
    let urgency = match notification.urgency {
        Urgency::Normal => "",
        Urgency::Low => " [low]",
        Urgency::Critical => " [critical]",
    };
    let mut text = format!("{when}  {}{urgency}: {}\n", notification.from(), notification.title);
    for line in notification.body.lines() {
        text.push_str("    ");
        text.push_str(line);
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_text() {
        assert_eq!(clean("  Backup done \t", 120, false), "Backup done");
        assert_eq!(clean("a\tb\nc\r\n\x1b[2Jd", 120, false), "a b c ?[2Jd");
        assert_eq!(clean("one\r\ntwo\x07", 120, true), "one\ntwo?");
        assert_eq!(clean("abcdefghij", 8, false), "abcde...");
        assert_eq!(clean("é".repeat(10).as_str(), 10, false), "é".repeat(10));
    }

    #[test]
    fn makes_notifications() {
        let request = Request {
            ask: "send".into(),
            title: "Backup done".into(),
            body: "3 disks\n".into(),
            app: Some("backup".into()),
            ..Request::default()
        };
        let made = notification(&request, 4, "t".into(), 57, "notify-send").unwrap();
        assert_eq!(made.seconds, SECONDS_DEFAULT);
        assert_eq!(made.urgency, Urgency::Normal);
        assert_eq!(made.body, "3 disks");
        assert_eq!(made.from(), "notify-send (backup)");
        let critical = Request {
            urgency: Some(Urgency::Critical),
            seconds: Some(5),
            title: "Battery low".into(),
            ..Request::default()
        };
        let made = notification(&critical, 5, "t".into(), 9, "powerd").unwrap();
        assert_eq!((made.seconds, made.from().as_str()), (0, "powerd"));
        let long = Request { seconds: Some(3600), title: "x".into(), ..Request::default() };
        assert_eq!(notification(&long, 6, "t".into(), 9, "x").unwrap().seconds, SECONDS_MAX);
        let blank = Request { title: " \n ".into(), ..Request::default() };
        assert!(notification(&blank, 7, "t".into(), 9, "x").is_err());
        // A program's claimed name is shown after its own, never alone.
        let same = Request { app: Some("x".into()), title: "y".into(), ..Request::default() };
        assert_eq!(notification(&same, 8, "t".into(), 9, "x").unwrap().from(), "x");
    }

    #[test]
    fn requests_round_trip() {
        let request =
            Request { ask: "list".into(), count: Some(0), follow: true, ..Default::default() };
        let line = serde_json::to_string(&request).unwrap();
        assert_eq!(line, r#"{"ask":"list","count":0,"follow":true}"#);
        let send: Request =
            serde_json::from_str(r#"{"ask":"send","title":"t","urgency":"critical"}"#).unwrap();
        assert_eq!(send.urgency, Some(Urgency::Critical));
        assert!(serde_json::from_str::<Request>(r#"{"urgency":"loud"}"#).is_err());
    }

    #[test]
    fn limits_floods() {
        let mut limiter = Limiter::default();
        let start = Instant::now();
        for _ in 0..BURST {
            assert!(limiter.allow(41, start));
        }
        assert!(!limiter.allow(41, start + Duration::from_secs(1)));
        // Another process is not held up.
        assert!(limiter.allow(42, start + Duration::from_secs(1)));
        // A window later, it may send again.
        assert!(limiter.allow(41, start + WINDOW));
        assert_eq!(limiter.sent.get(&41).map(VecDeque::len), Some(1));
    }

    #[test]
    fn shows_notifications() {
        let notification = Notification {
            id: 1,
            time: "2026-10-30T23:40:01.123456Z".into(),
            process: 57,
            name: "notify-send".into(),
            app: None,
            urgency: Urgency::Critical,
            title: "Disk full".into(),
            body: "/volumes/usb\nis full".into(),
            seconds: 0,
            terminal: None,
        };
        assert_eq!(
            show(&notification),
            "2026-10-30 23:40:01  notify-send [critical]: Disk full\n    /volumes/usb\n    is full\n"
        );
    }

    #[test]
    fn expire_times() {
        assert_eq!(expire_time("0").unwrap(), 0);
        assert_eq!(expire_time("1").unwrap(), 1);
        assert_eq!(expire_time("8000").unwrap(), 8);
        assert_eq!(expire_time("99999999999").unwrap(), SECONDS_MAX);
        assert!(expire_time("-1").is_err());
    }
}
