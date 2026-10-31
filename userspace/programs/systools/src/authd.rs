// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! authd(8): Dukovo's accounts (ADR 0218).
//!
//! authd alone changes the account files (`accounts`): the names in
//! `/system/settings/passwd` and `group`, which everyone reads, and the
//! Argon2id hashes of the passwords in `/system/state/authd/passwords`,
//! which no other program's view holds. Programs ask it over
//! `/system/live/authd/socket` (`accounts::request`): whether a password
//! is right, to change one, and to make an account, with its group and
//! its home. It tells who asks by the user the kernel gives for the
//! connection (`socket_state`, SO_PEERCRED), never by what the request
//! says. Each change, and each wrong password, is a security event in
//! the audit log (ADR 0211).
//!
//! Guessing is slowed down, never locked out: after a wrong password, the
//! next try at that account from the same user waits, 2 seconds after
//! each of the first three and twice as long after each one more, up to a
//! minute, and the answer to the wrong one comes only then. A right one
//! clears it. Hashes are made one at a time, so a burst of tries cannot
//! take more memory than one hash's 64 MiB.
//!
//! authd answers on one thread: each connection sends one request and
//! has [`REQUEST_TIME`] to send it, and gets one answer.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use accounts::hash::{self, Cost};
use accounts::request::{Answer, REQUEST_MAX, Request};
use accounts::{Group, Password, Table, User};

use crate::args::Args;
use crate::sys;
use crate::util::{self, Fail};

/// How long a connection has to send its request.
const REQUEST_TIME: Duration = Duration::from_secs(10);

/// The most connections at a time, and from one process.
const CLIENTS_MAX: usize = 64;
const CLIENTS_PER_PROCESS: usize = 4;

/// What a wrong password costs the next try, after the first
/// [`FREE_TRIES`]: twice as long each time, up to [`LONGEST_WAIT`].
const WAIT: Duration = Duration::from_secs(2);
const FREE_TRIES: u32 = 3;
const LONGEST_WAIT: Duration = Duration::from_secs(60);
/// A record of wrong passwords this old is forgotten.
const FORGET: Duration = Duration::from_mins(15);

/// The process at the other end of a connection, as the kernel gives it.
#[derive(Clone, Copy, Debug)]
struct Peer {
    process: u64,
    user: u32,
}

impl Peer {
    fn of(stream: &UnixStream) -> Peer {
        let slot = sys::slot_of(stream.as_raw_fd());
        match radix_abi::file::socket_state(&sys::SYS, slot) {
            Ok(state) => Peer { process: state.peer, user: state.user },
            // Nobody: no number of a user anyone has.
            Err(_) => Peer { process: 0, user: u32::MAX },
        }
    }

    /// Whether it acts for root: from 13b.6, an administrator inside
    /// elevate.
    fn is_root(self) -> bool {
        self.user == 0
    }

    /// Where its tries come from, for slowing them down.
    fn source(self) -> String {
        format!("user {}", self.user)
    }
}

/// The wrong passwords given for an account from one source.
struct Wrong {
    count: u32,
    /// When the next try is checked.
    next: Instant,
}

/// How long the try after the `count`th wrong one in a row waits.
fn wait_after(count: u32) -> Duration {
    let doublings = count.saturating_sub(FREE_TRIES).min(10);
    (WAIT * (1 << doublings)).min(LONGEST_WAIT)
}

/// A connection.
struct Client {
    stream: UnixStream,
    peer: Peer,
    request: Vec<u8>,
    /// Whether its request has come whole, or will not be read.
    ended: bool,
    /// Until when it may send; then until when its answer may wait to be
    /// read.
    deadline: Instant,
    /// A request that waits for its turn, and when that is.
    waiting: Option<(Request, Instant)>,
    /// Its answer, and when to send it.
    answer: Option<(Vec<u8>, Instant)>,
    done: bool,
}

/// authd's state.
struct Authd {
    listener: UnixListener,
    clients: Vec<Client>,
    wrong: HashMap<(String, String), Wrong>,
}

/// Set by SIGTERM: authd ends.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// A security event, for the audit log: Spark passes a line of a
/// service's that starts with `<86>` on as one (ADR 0211).
macro_rules! audit {
    ($($arg:tt)*) => { println!("<86>authd: {}", format_args!($($arg)*)) };
}

/// The longest name a line of authd's shows.
const SHOWN_MAX: usize = 64;

/// `name`, from a request, as a line shows it: it may hold anything, and
/// a newline in it would start a line of its own, which Spark would take
/// for another event. A control character shows as `?`.
fn shown(name: &str) -> String {
    name.chars().take(SHOWN_MAX).map(|c| if c.is_control() { '?' } else { c }).collect()
}

/// The text of the account file at `path`, or none if there is none.
fn read_text(path: &str) -> io::Result<String> {
    match fs::read_to_string(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        other => other,
    }
}

/// The three account files, as they are now: read again for each
/// request, so that an edit by hand counts at once.
struct Files {
    users: Table<User>,
    groups: Table<Group>,
    passwords: Table<Password>,
}

impl Files {
    fn read() -> Result<Files, String> {
        let read = |path: &str| read_text(path).map_err(|e| format!("cannot read {path}: {e}"));
        Ok(Files {
            users: accounts::users(&read(accounts::PASSWD)?),
            groups: accounts::groups(&read(accounts::GROUP)?),
            passwords: accounts::passwords(&read(accounts::PASSWORDS)?),
        })
    }

    fn user(&self, name: &str) -> Option<&User> {
        self.users.entries().find(|u| u.name == name)
    }

    /// The hash of `name`'s password: `None` for an account with no line
    /// in the passwords file, which no password opens.
    fn hash(&self, name: &str) -> Option<&str> {
        self.passwords.entries().find(|p| p.name == name).map(|p| p.hash.as_str())
    }
}

/// Writes `text` to the file at `path` with `mode`, owned by root: to a
/// new file beside it first, made to last, then put in its place, so that
/// the file is whole whatever happens.
fn replace(path: &str, text: &str, mode: u32) -> Result<(), String> {
    let new = format!("{path}.new");
    let written = (|| {
        let mut file =
            OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(&new)?;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&new, path)
    })();
    written.map_err(|err| {
        let _ = fs::remove_file(&new);
        format!("cannot write {path}: {}", util::strip_os_error(&err))
    })
}

/// 16 bytes for a salt.
fn salt() -> Result<[u8; hash::SALT], String> {
    let mut salt = [0u8; hash::SALT];
    File::open("/devices/urandom")
        .and_then(|mut random| random.read_exact(&mut salt))
        .map_err(|err| format!("cannot read /devices/urandom: {err}"))?;
    Ok(salt)
}

/// The hash of `password`, for the passwords file: empty for an empty
/// password, which then opens the account with nothing typed, as Linux's
/// `passwd -d` leaves it.
fn hash_of(password: &[u8]) -> Result<String, String> {
    if password.is_empty() {
        return Ok(String::new());
    }
    hash::hash(password, &salt()?, Cost::DEFAULT)
}

/// Whether `password` is `name`'s: an error for an account authd cannot
/// check, whose line is missing or unreadable.
fn password_is(files: &Files, name: &str, password: &[u8]) -> Result<bool, String> {
    let locked = || {
        format!(
            "{name}'s password cannot be checked: its line in {} is missing or broken",
            accounts::PASSWORDS
        )
    };
    match files.hash(name) {
        None => Err(locked()),
        Some("") => Ok(password.is_empty()),
        Some(phc) => hash::verify(password, phc).map_err(|_| locked()),
    }
}

/// Gives the directory at `path`, root's, to `user` with `mode`; a link
/// there is not followed.
fn give(path: &Path, user: &User, mode: u32) -> io::Result<()> {
    std::os::unix::fs::lchown(path, Some(user.uid), Some(user.gid))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

/// Makes `user`'s home and its folders: the home only the user may
/// enter, the folders as a new file's, and `app-data`, which holds apps'
/// own data, the user's alone. A home already there is left as it is.
///
/// The folders are made and given while the home is still root's 0700,
/// and the home is given last, so that the user, who may be running
/// already, can put nothing in authd's way: no link where a folder is
/// to be.
fn make_home(user: &User) -> Result<&'static str, String> {
    let home = Path::new(&user.home);
    let failed =
        |err: io::Error| format!("cannot make {}: {}", home.display(), util::strip_os_error(&err));
    match fs::DirBuilder::new().mode(0o700).create(home) {
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            return Ok("; its home was there already, and is as it was");
        }
        other => other.map_err(failed)?,
    }
    for folder in accounts::HOME_FOLDERS {
        let mode = if folder == "app-data" { 0o700 } else { 0o755 };
        let path = home.join(folder);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .and_then(|()| give(&path, user, mode))
            .map_err(failed)?;
    }
    give(home, user, 0o700).map_err(failed)?;
    Ok("")
}

impl Authd {
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
            let peer = Peer::of(&stream);
            let held = self.clients.iter().filter(|c| c.peer.process == peer.process).count();
            let mut client = Client {
                stream,
                peer,
                request: Vec::new(),
                ended: false,
                deadline: now + REQUEST_TIME,
                waiting: None,
                answer: None,
                done: false,
            };
            if held >= CLIENTS_PER_PROCESS {
                let why =
                    format!("more than {CLIENTS_PER_PROCESS} requests at once from one program");
                client.ended = true;
                client.answer(&Answer::Refused(why), now, now);
            }
            self.clients.push(client);
        }
    }

    /// Reads what client `i` sent; once it has shut its side, answers.
    /// One whose request came, and that hangs up, has gone, and is
    /// answered no more.
    fn read(&mut self, i: usize, now: Instant) {
        let mut buffer = [0u8; 1024];
        loop {
            let client = &mut self.clients[i];
            if client.ended {
                client.done = true;
                return;
            }
            match client.stream.read(&mut buffer) {
                Ok(0) => {
                    client.ended = true;
                    let mut bytes = std::mem::take(&mut client.request);
                    let decoded = Request::decode(&bytes);
                    accounts::wipe(&mut bytes);
                    match decoded {
                        Ok(request) => self.take(i, request, now),
                        Err(why) => self.clients[i].answer(&Answer::Refused(why), now, now),
                    }
                    return;
                }
                Ok(n) => {
                    client.request.extend_from_slice(&buffer[..n]);
                    if client.request.len() > REQUEST_MAX {
                        accounts::wipe(&mut client.request);
                        client.ended = true;
                        let why = "a request longer than authd reads".to_string();
                        client.answer(&Answer::Refused(why), now, now);
                        return;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => {
                    client.done = true;
                    return;
                }
            }
        }
    }

    /// Takes client `i`'s request: now, or when the wrong passwords given
    /// before for the account it checks let it.
    fn take(&mut self, i: usize, request: Request, now: Instant) {
        let peer = self.clients[i].peer;
        let checks = match &request {
            Request::Check { name, .. } => Some(name),
            Request::Passwd { name, .. } if !peer.is_root() => Some(name),
            _ => None,
        };
        let turn = checks
            .and_then(|name| self.wrong.get(&(name.clone(), peer.source())))
            .map(|wrong| wrong.next)
            .filter(|&next| next > now);
        match turn {
            // It may wait longer than a connection's time to send: its
            // answer is due then.
            Some(at) => {
                let client = &mut self.clients[i];
                client.waiting = Some((request, at));
                client.deadline = client.deadline.max(at + REQUEST_TIME);
            }
            None => self.run(i, request, now),
        }
    }

    /// Carries out client `i`'s request.
    fn run(&mut self, i: usize, mut request: Request, now: Instant) {
        let peer = self.clients[i].peer;
        let (answer, at) = match Files::read() {
            Err(why) => (Answer::Refused(why), now),
            Ok(files) => match &request {
                Request::Check { name, password } => self.check(&files, name, password, peer, now),
                Request::Passwd { name, current, new } => {
                    self.passwd(&files, name, current, new, peer, now)
                }
                Request::Add { name, full_name, admin, password } => {
                    (add(files, name, full_name, *admin, password, peer), now)
                }
            },
        };
        request.wipe();
        self.clients[i].answer(&answer, at, now);
    }

    /// Checks `password` against `name`'s, from `peer`: a wrong one, or
    /// an account there is not, is answered only when the next try may
    /// be made, and the same way, so that neither tells which it was.
    fn check_password(
        &mut self,
        files: &Files,
        name: &str,
        password: &[u8],
        peer: Peer,
        now: Instant,
    ) -> Result<(), (Answer, Instant)> {
        let right = match files.user(name) {
            // The hash is made all the same, so that the time taken does
            // not say whether the account is there.
            None => {
                let _ = hash::verify(password, DECOY);
                Ok(false)
            }
            Some(_) => password_is(files, name, password),
        };
        let key = (name.to_string(), peer.source());
        match right {
            Ok(true) => {
                self.wrong.remove(&key);
                Ok(())
            }
            Ok(false) => {
                let wrong = self.wrong.entry(key).or_insert(Wrong { count: 0, next: now });
                wrong.count += 1;
                wrong.next = now + wait_after(wrong.count);
                audit!(
                    "a wrong password for {}, from process {} (user {}); {} in a row",
                    shown(name),
                    peer.process,
                    peer.user,
                    wrong.count
                );
                Err((Answer::Refused("the name or the password is wrong".into()), wrong.next))
            }
            Err(why) => Err((Answer::Refused(why), now)),
        }
    }

    fn check(
        &mut self,
        files: &Files,
        name: &str,
        password: &[u8],
        peer: Peer,
        now: Instant,
    ) -> (Answer, Instant) {
        // A program checks only the password of the user it runs for, as
        // a screen lock does; sign-in, which checks anyone's, runs as
        // root. The answer is the same whether the account is there.
        if !peer.is_root() && files.user(name).is_none_or(|u| u.uid != peer.user) {
            let why = "a program checks only the password of the user it runs for";
            return (Answer::Refused(why.into()), now);
        }
        match self.check_password(files, name, password, peer, now) {
            Ok(()) => (Answer::Done(String::new()), now),
            Err(refused) => refused,
        }
    }

    /// Changes `name`'s password to `new`: a user's own, given the one it
    /// has now, or anyone's, for root.
    fn passwd(
        &mut self,
        files: &Files,
        name: &str,
        current: &[u8],
        new: &[u8],
        peer: Peer,
        now: Instant,
    ) -> (Answer, Instant) {
        let Some(user) = files.user(name) else {
            return (Answer::Refused(format!("there is no account {}", shown(name))), now);
        };
        if !peer.is_root() {
            if peer.user != user.uid {
                let why = "only its owner, or an administrator inside elevate, changes a password";
                return (Answer::Refused(why.into()), now);
            }
            if let Err(refused) = self.check_password(files, name, current, peer, now) {
                return refused;
            }
        }
        let changed = hash_of(new).and_then(|hash| {
            let mut passwords = files.passwords.clone();
            if let Some(line) = passwords.entries_mut().find(|p| p.name == name) {
                line.hash = hash;
            } else {
                passwords.push(Password { name: name.into(), hash });
            }
            replace(accounts::PASSWORDS, &passwords.text(), 0o600)
        });
        if let Err(why) = changed {
            return (Answer::Refused(why), now);
        }
        let empty = if new.is_empty() { ", to an empty one" } else { "" };
        audit!(
            "the password of {name} was changed{empty}, by process {} (user {})",
            peer.process,
            peer.user
        );
        (Answer::Done(String::new()), now)
    }
}

/// A hash of nothing anyone types, at the cost of every password's,
/// checked for a name with no account: `"\0no account"` with a salt of
/// sixteen 0x5a bytes, at `Cost::DEFAULT`, which a test checks.
const DECOY: &str = "$argon2id$v=19$m=65536,t=3,p=4$WlpaWlpaWlpaWlpaWlpaWg$TyqYhTrIdMW7XU4wCjpoNu3vkL+MZnQHSLWCDwvtyOE";

/// Makes the account `name`: its user, its own group of the same number,
/// its password's hash, and its home; in the administrators' group if
/// `admin`. Root's to ask (from 13b.6, an administrator inside elevate).
fn add(
    files: Files,
    name: &str,
    full_name: &str,
    admin: bool,
    password: &[u8],
    peer: Peer,
) -> Answer {
    if !peer.is_root() {
        return Answer::Refused("only an administrator, inside elevate, makes accounts".into());
    }
    let Files { mut users, mut groups, mut passwords } = files;
    let user = match accounts::add(&mut users, &mut groups, name, full_name, admin) {
        Ok(user) => user,
        Err(why) => return Answer::Refused(why),
    };
    let number = user.uid;
    // A line left from an account of that name before goes.
    passwords.retain_entries(|p| p.name != name);
    // The hash and the group first, the user last: an account is there
    // once its user is, and a failure before leaves none.
    let made = hash_of(password)
        .and_then(|hash| {
            passwords.push(Password { name: name.into(), hash });
            replace(accounts::PASSWORDS, &passwords.text(), 0o600)
        })
        .and_then(|()| replace(accounts::GROUP, &groups.text(), 0o644))
        .and_then(|()| replace(accounts::PASSWD, &users.text(), 0o644))
        .and_then(|()| make_home(&user));
    match made {
        Ok(home_note) => {
            let kind = if admin { "an administrator's" } else { "a user's" };
            audit!(
                "made the account {name} ({number}), {kind}, for process {} (user {})",
                peer.process,
                peer.user
            );
            Answer::Done(format!("made {name}, number {number}, home {}{home_note}", user.home))
        }
        Err(why) => Answer::Refused(why),
    }
}

impl Client {
    /// Has it get `answer` at `at`, then close.
    fn answer(&mut self, answer: &Answer, at: Instant, now: Instant) {
        self.waiting = None;
        self.answer = Some((answer.encode().into_bytes(), at));
        self.deadline = at.max(now) + REQUEST_TIME;
    }

    /// Sends its answer if it is time; then it is done.
    fn send(&mut self, now: Instant) {
        let Some((bytes, at)) = &mut self.answer else { return };
        if *at > now {
            return;
        }
        while !bytes.is_empty() {
            match self.stream.write(bytes) {
                Ok(0) => break,
                Ok(n) => {
                    bytes.drain(..n);
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => break,
            }
        }
        self.done = true;
    }
}

impl Authd {
    fn run_loop(&mut self) -> io::Result<()> {
        loop {
            if STOP.load(Ordering::Relaxed) {
                return Ok(());
            }
            let now = Instant::now();
            // Requests whose turn came, and answers due.
            for i in 0..self.clients.len() {
                let due = self.clients[i].waiting.as_ref().is_some_and(|(_, at)| *at <= now);
                if due && let Some((request, _)) = self.clients[i].waiting.take() {
                    self.run(i, request, now);
                }
            }
            let now = Instant::now();
            for client in &mut self.clients {
                client.send(now);
            }
            self.wrong.retain(|_, wrong| now.saturating_duration_since(wrong.next) < FORGET);
            for client in &mut self.clients {
                if client.deadline <= now {
                    client.done = true;
                }
            }
            self.clients.retain(|c| !c.done);
            let listening = self.clients.len() < CLIENTS_MAX;
            let mut fds = vec![libc::pollfd {
                fd: if listening { self.listener.as_raw_fd() } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            }];
            // A client whose request came is not polled to read, as its
            // end is always there to read; it is still told hung up.
            for client in &self.clients {
                let due = client.answer.as_ref().is_some_and(|(_, at)| *at <= now);
                let read = if client.ended { 0 } else { libc::POLLIN };
                fds.push(libc::pollfd {
                    fd: client.stream.as_raw_fd(),
                    events: read | if due { libc::POLLOUT } else { 0 },
                    revents: 0,
                });
            }
            let next = self
                .clients
                .iter()
                .flat_map(|c| {
                    [
                        Some(c.deadline),
                        c.waiting.as_ref().map(|w| w.1),
                        c.answer.as_ref().map(|a| a.1),
                    ]
                })
                .flatten()
                .min();
            let timeout = next.map_or(-1, |at| {
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
            for (i, entry) in fds[1..].iter().enumerate() {
                if entry.revents & libc::POLLOUT != 0 {
                    self.clients[i].send(now);
                }
                if entry.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                    self.read(i, now);
                }
            }
            if fds[0].revents != 0 {
                self.accept(now);
            }
        }
    }
}

/// Makes `path`, a directory, with `mode` if it is not there, and gives
/// it that mode if it is.
fn own_dir(path: &str, mode: u32) -> Result<(), Fail> {
    match fs::DirBuilder::new().recursive(true).mode(mode).create(path) {
        Ok(()) => {}
        Err(err) => return Err(util::failed(&path, err)),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| util::failed(&path, e))
}

/// authd(8).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    if let Some(arg) = args.next()? {
        return Err(args.unknown(&arg));
    }
    // SAFETY: setting a signal's disposition touches no memory; no other
    // thread runs. A client that goes away must not end authd: the write
    // to it fails instead.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    // SAFETY: all zeros is a valid sigaction: numbers, a signal set and a
    // function address.
    let mut action: libc::sigaction = unsafe { util::zeroed() };
    action.sa_sigaction = on_term as extern "C" fn(libc::c_int) as *const () as usize;
    // SAFETY: the handler only stores to an atomic.
    unsafe { libc::sigaction(libc::SIGTERM, &raw const action, std::ptr::null_mut()) };
    // Its own files, which only it reads; the hashes are root's 0600 in
    // there too.
    own_dir(accounts::STATE, 0o700)?;
    let dir = Path::new(accounts::SOCKET).parent().unwrap_or(Path::new("/"));
    own_dir(&dir.to_string_lossy(), 0o755)?;
    let _ = fs::remove_file(accounts::SOCKET);
    let listener =
        UnixListener::bind(accounts::SOCKET).map_err(|e| util::failed(&accounts::SOCKET, e))?;
    // Anyone may ask: it tells who asks by the connection.
    fs::set_permissions(accounts::SOCKET, fs::Permissions::from_mode(0o666))
        .map_err(|e| util::failed(&accounts::SOCKET, e))?;
    listener.set_nonblocking(true).map_err(|e| util::failed(&accounts::SOCKET, e))?;
    // An account with no home gets one: one `ob image --user` made, as
    // every file in an image is root's, or one whose home went.
    let people: Vec<User> = Files::read()
        .map_or_default(|files| files.users.entries().filter(|u| u.is_person()).cloned().collect());
    for user in people.iter().filter(|u| fs::symlink_metadata(&u.home).is_err()) {
        match make_home(user) {
            Ok(_) => eprintln!("authd: made the home {} of {}", user.home, user.name),
            Err(why) => eprintln!("authd: {why}"),
        }
    }
    eprintln!("authd: {} accounts; answering at {}", people.len(), accounts::SOCKET);
    let mut authd = Authd { listener, clients: Vec::new(), wrong: HashMap::new() };
    authd.run_loop().map_err(|e| util::failed(&"authd", e))?;
    let _ = fs::remove_file(accounts::SOCKET);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_decoy_costs_what_a_password_does() {
        let made = hash::hash(b"\0no account", &[0x5a; hash::SALT], Cost::DEFAULT);
        assert_eq!(made.as_deref(), Ok(DECOY));
    }

    #[test]
    fn names_show_on_one_line() {
        assert_eq!(shown("alice"), "alice");
        assert_eq!(shown("x\n<86>spark: forged"), "x?<86>spark: forged");
        assert_eq!(shown("\u{1b}[2J\r\t"), "?[2J??");
        assert_eq!(shown(&"é".repeat(100)).chars().count(), SHOWN_MAX);
    }

    #[test]
    fn waits_grow_after_the_first_tries() {
        let waits: Vec<u64> = (1..=9).map(|n| wait_after(n).as_secs()).collect();
        assert_eq!(waits, [2, 2, 2, 4, 8, 16, 32, 60, 60]);
        assert_eq!(wait_after(1000), LONGEST_WAIT);
    }
}
