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
//!
//! It also runs commands as an administrator for elevate(8) (ADR 0219):
//! for an administrator, after their password unless their terminal
//! pane typed it in the last 15 minutes, it starts the command as its
//! own child with the admin set (`elevate::start`), passes it the
//! signals elevate sends, and tells elevate when it stops and ends.
//!
//! The password for elevate, in a pane on one of the consoles, is asked
//! by consoled (ADR 0251), drawn where no program can draw and typed
//! where no program reads: consoled's `prompts` connection stays open,
//! authd writes it an `ask` line for each password it wants, and holds
//! the elevate request, with its descriptors, until consoled's
//! `prompted` answer, or for at most [`PROMPT_TIME`].

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

use accounts::hash::{self, Cost};
use accounts::request::{self, Answer, Mode, Prompt, Prompted, REQUEST_MAX, Relay, Request};
use accounts::{DISABLED, Group, Password, Table, User};
use radix_abi::names;

use crate::args::Args;
use crate::elevate::{self, Plan};
use crate::util::{self, Fail};
use crate::{proc, signin, sys};

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

/// How long a password asked on a console may take to be typed, as sudo's
/// `passwd_timeout`.
const PROMPT_TIME: Duration = Duration::from_mins(5);

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
    /// The descriptors sent with its request: a session's terminal, or
    /// an elevated command's three and its pipe.
    descriptors: Vec<OwnedFd>,
    /// The first process of the session started for it, or the elevated
    /// command, whose end it waits for.
    session: Option<i32>,
    /// The pipe elevate passes signals on through, while its command
    /// runs, and what came of a line not whole yet.
    relay: Option<(OwnedFd, Vec<u8>)>,
    /// Whether it is consoled's `prompts` connection.
    prompter: bool,
    /// Its request, while the password for it is asked on a console.
    asked: Option<Asked>,
    done: bool,
}

/// A request held while its password is asked on a console.
struct Asked {
    /// The prompt's number.
    id: u64,
    request: Request,
    /// When it is given up.
    until: Instant,
}

/// A password to ask on a console, for the request being run: the
/// terminal, whose it is, and what it runs.
struct Prompting {
    terminal: String,
    name: String,
    shown: String,
}

/// A terminal pane, as elevate unlocks it: a session's terminal, by the
/// user who asks, the session, and when its leader started, so that a
/// later session that gets the same numbers is another pane.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Pane {
    user: u32,
    terminal: String,
    session: u64,
    leader_started_ns: u64,
}

impl Pane {
    /// The pane of the process at the other end of a connection: its
    /// session's terminal, if it has one, and acts for the user the
    /// connection gives.
    fn of(peer: Peer) -> Option<Pane> {
        let process = proc::process(peer.process).ok()?;
        let terminal = process.terminal.filter(|t| t != "none")?;
        if process.user != peer.user {
            return None;
        }
        let leader = proc::process(process.session).ok()?;
        Some(Pane {
            user: peer.user,
            terminal,
            session: process.session,
            leader_started_ns: leader.started_ns,
        })
    }
}

/// authd's state.
struct Authd {
    listener: UnixListener,
    clients: Vec<Client>,
    wrong: HashMap<(String, String), Wrong>,
    /// The names capability Spark gave it, to start sessions with.
    names: Option<u32>,
    /// The sessions it started that run, by their first process, and
    /// the elevated commands.
    sessions: HashMap<i32, Session>,
    /// The panes elevate is unlocked in, and until when.
    unlocked: HashMap<Pane, Instant>,
    /// The number of the next password prompt on a console.
    next_prompt: u64,
    /// Set while running a request whose password is to be asked on a
    /// console.
    prompting: Option<Prompting>,
    /// The pipe's read end that SIGCHLD wakes it with.
    children: File,
}

/// Set by SIGTERM: authd ends.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// The write end of the pipe SIGCHLD wakes authd's loop with.
static CHILDREN: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_child(_: libc::c_int) {
    let byte = 1u8;
    // SAFETY: write(2) is async-signal-safe; the pipe does not block.
    unsafe { libc::write(CHILDREN.load(Ordering::Relaxed), (&raw const byte).cast(), 1) };
}

/// A session authd started, or an elevated command: whose it is, and on
/// what terminal, for the audit log.
struct Session {
    name: String,
    terminal: String,
    /// Whether it is an elevated command.
    elevated: bool,
}

/// How long a connection whose session runs may wait: as long as it
/// runs.
const SESSION_TIME: Duration = Duration::from_hours(100 * 365 * 24);

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

    /// The hash of `name`'s password, without the mark of a disabled
    /// account: `None` for an account with no line in the passwords file,
    /// which no password opens.
    fn hash(&self, name: &str) -> Option<&str> {
        self.passwords.entries().find(|p| p.name == name).map(Password::own_hash)
    }

    /// Whether `name`'s account is disabled (ADR 0220).
    fn disabled(&self, name: &str) -> bool {
        self.passwords.entries().any(|p| p.name == name && p.is_disabled())
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
fn make_home(user: &User) -> Result<String, String> {
    let home = Path::new(&user.home);
    let failed =
        |err: io::Error| format!("cannot make {}: {}", home.display(), util::strip_os_error(&err));
    match fs::DirBuilder::new().mode(0o700).create(home) {
        // A removed account's, kept, is still the removed account's
        // (ADR 0220): the administrator decides.
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let owner = fs::symlink_metadata(home).map_or(user.uid, |m| m.uid());
            return Ok(if owner == user.uid {
                "; its home was there already, and is as it was".into()
            } else {
                format!(
                    "; its home was there already, and is user {owner}'s: `elevate chown -R \
                     {}: {}` gives it to {}",
                    user.name,
                    home.display(),
                    user.name
                )
            });
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
    Ok(String::new())
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
                descriptors: Vec::new(),
                session: None,
                relay: None,
                prompter: false,
                asked: None,
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
            match signin::receive(&client.stream, &mut buffer, &mut client.descriptors) {
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
        let own;
        let checks = match &request {
            Request::Check { name, .. } | Request::Session { name, .. } => Some(name),
            Request::Passwd { name, .. } if !peer.is_root() => Some(name),
            Request::Elevate { password: Some(_), .. }
            | Request::Unlock { password: Some(_), .. } => {
                own = util::user_name(peer.user);
                Some(&own)
            }
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
                Request::NeedsOwner => {
                    let needs = if needs_owner(&files) { "yes" } else { "no" };
                    (Answer::Done(needs.into()), now)
                }
                Request::Owner { name, full_name, password } => {
                    if !peer.is_root() {
                        let why =
                            "only a sign-in program, which runs as root, makes the owner's account";
                        (Answer::Refused(why.into()), now)
                    } else if !needs_owner(&files) {
                        let why = "this computer has an account to sign in with already";
                        (Answer::Refused(why.into()), now)
                    } else {
                        (add(files, name, full_name, true, password, peer), now)
                    }
                }
                Request::Reset { name, password } => {
                    (self.reset(&files, name, password, peer), now)
                }
                Request::Disable { name } => (self.disable(&files, name, peer), now),
                Request::Enable { name } => (enable(&files, name, peer), now),
                Request::Remove { name } => (self.remove(files, name, peer), now),
                Request::Rename { name, new } => (self.rename(files, name, new, peer), now),
                Request::Set { name, full_name, admin } => {
                    (self.set(files, name, full_name.as_deref(), *admin, peer), now)
                }
                Request::Session { name, password, term } => {
                    let (answer, at) = self.session(i, &files, name, password, term, peer, now);
                    // A wrong password is in the audit log already; any
                    // other refusal says why a console could not sign in.
                    if let Answer::Refused(why) = &answer
                        && at == now
                    {
                        eprintln!("authd: no session for {}: {why}", shown(name));
                    }
                    (answer, at)
                }
                Request::Elevate { mode, password, ask, term, directory, language, command } => {
                    let asked = Elevation { mode: *mode, term, directory, language, command };
                    self.elevate(i, &files, &asked, password.as_deref(), *ask, peer, now)
                }
                Request::Unlock { password, ask } => {
                    self.unlock(&files, password.as_deref(), *ask, peer, now)
                }
                Request::Forget => {
                    if let Some(pane) = Pane::of(peer) {
                        self.unlocked.remove(&pane);
                    }
                    (Answer::Done(String::new()), now)
                }
                Request::Prompts => self.prompts(i, peer, now),
                Request::Prompted { id, typed } => {
                    let from =
                        self.clients.iter().any(|c| c.prompter && c.peer.process == peer.process);
                    if peer.is_root() && from {
                        self.prompted(*id, typed, now);
                        (Answer::Done(String::new()), now)
                    } else {
                        let why = "only the program that asks for passwords on the consoles answers \
                                   them";
                        (Answer::Refused(why.into()), now)
                    }
                }
            },
        };
        // A password to ask on a console: the request waits for it.
        if let Some(prompting) = self.prompting.take() {
            self.ask_console(i, request, prompting, now);
            return;
        }
        request.wipe();
        self.clients[i].answer(&answer, at, now);
    }

    /// The `prompts` connection, client `i`'s: consoled's, which runs as
    /// root. A later one takes over from an earlier one, whose prompts
    /// are then asked by elevate.
    fn prompts(&mut self, i: usize, peer: Peer, now: Instant) -> (Answer, Instant) {
        if !peer.is_root() {
            let why = "only consoled, which runs as root, asks for passwords on the consoles";
            return (Answer::Refused(why.into()), now);
        }
        for (j, client) in self.clients.iter_mut().enumerate() {
            if client.prompter && j != i {
                client.done = true;
            }
        }
        self.clients[i].prompter = true;
        eprintln!("authd: process {} asks for passwords on the consoles", peer.process);
        (Answer::Done(String::new()), now)
    }

    /// Whether a program asks for passwords on the consoles.
    fn has_prompter(&self) -> bool {
        self.clients.iter().any(|c| c.prompter && !c.done)
    }

    /// Has the line `prompt` go to the program that asks for passwords on
    /// the consoles.
    fn tell_prompter(&mut self, prompt: &Prompt, now: Instant) {
        if let Some(prompter) = self.clients.iter_mut().find(|c| c.prompter && !c.done) {
            prompter.tell(prompt.encode().as_bytes(), now);
        }
    }

    /// Holds client `i`'s `request` while its password is asked on a
    /// console.
    fn ask_console(&mut self, i: usize, request: Request, prompting: Prompting, now: Instant) {
        let id = self.next_prompt;
        self.next_prompt += 1;
        let Prompting { terminal, name, shown } = prompting;
        self.tell_prompter(&Prompt::Ask { id, terminal, name, shown }, now);
        let until = now + PROMPT_TIME;
        let client = &mut self.clients[i];
        client.asked = Some(Asked { id, request, until });
        client.deadline = until + REQUEST_TIME;
    }

    /// What was typed at a console for prompt `id`: the request that waits
    /// for it is taken with it as its password, or answered.
    fn prompted(&mut self, id: u64, typed: &Prompted, now: Instant) {
        let Some(j) =
            self.clients.iter().position(|c| c.asked.as_ref().is_some_and(|a| a.id == id))
        else {
            return;
        };
        let Some(Asked { mut request, .. }) = self.clients[j].asked.take() else { return };
        let client = &mut self.clients[j];
        match typed {
            Prompted::Password(typed) => {
                if let Request::Elevate { password, ask, .. } | Request::Unlock { password, ask } =
                    &mut request
                {
                    *password = Some(typed.clone());
                    *ask = false;
                }
                client.deadline = now + REQUEST_TIME;
                self.take(j, request, now);
            }
            Prompted::Cancelled => {
                client.answer(&Answer::Refused("no password was typed".into()), now, now);
            }
            Prompted::Elsewhere => {
                client.answer(&Answer::Done(request::NEEDS_PASSWORD.into()), now, now);
            }
        }
    }

    /// Gives up the prompts whose time is up, tells consoled of those
    /// whose asker went, and has elevate ask for the rest itself when
    /// consoled went.
    fn tidy_prompts(&mut self, now: Instant) {
        let prompter = self.has_prompter();
        let mut gone = Vec::new();
        for client in &mut self.clients {
            let Some(asked) = &client.asked else { continue };
            if client.done {
                gone.push(asked.id);
                client.asked = None;
            } else if !prompter {
                client.asked = None;
                client.answer(&Answer::Done(request::NEEDS_PASSWORD.into()), now, now);
            } else if asked.until <= now {
                gone.push(asked.id);
                client.asked = None;
                let why =
                    format!("no password was typed in {} minutes", PROMPT_TIME.as_secs() / 60);
                client.answer(&Answer::Refused(why), now, now);
            }
        }
        for id in gone {
            self.tell_prompter(&Prompt::Done { id }, now);
        }
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
            // Said only to someone who knows the password, as Linux's
            // login tells of an expired account.
            Ok(true) if files.disabled(name) => {
                self.wrong.remove(&key);
                audit!(
                    "refused {} to process {} (user {}): the account is disabled",
                    shown(name),
                    peer.process,
                    peer.user
                );
                let why = format!(
                    "the account {} is disabled: an administrator enables it with elevate user \
                     enable {}",
                    shown(name),
                    shown(name)
                );
                Err((Answer::Refused(why), now))
            }
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
                Err((Answer::Refused(request::WRONG_PASSWORD.into()), wrong.next))
            }
            Err(why) => Err((Answer::Refused(why), now)),
        }
    }

    /// Starts `name`'s session on the terminal client `i` sent, if
    /// `password` is theirs: root's to ask, as sign-in programs run as
    /// root. The client then waits for the session's end.
    #[allow(clippy::too_many_arguments)]
    fn session(
        &mut self,
        i: usize,
        files: &Files,
        name: &str,
        password: &[u8],
        term: &str,
        peer: Peer,
        now: Instant,
    ) -> (Answer, Instant) {
        let mut descriptors = std::mem::take(&mut self.clients[i].descriptors);
        if !peer.is_root() {
            let why = "only a sign-in program, which runs as root, starts sessions";
            return (Answer::Refused(why.into()), now);
        }
        let terminal = (descriptors.len() == 1).then(|| descriptors.remove(0));
        let Some(terminal) = terminal.filter(signin::is_terminal) else {
            return (Answer::Refused("a session request comes with its terminal".into()), now);
        };
        let Some(names) = self.names else {
            let why = "authd holds no names capability to start sessions with: its service file \
                       needs `names = true`";
            return (Answer::Refused(why.into()), now);
        };
        if let Err(refused) = self.check_password(files, name, password, peer, now) {
            return refused;
        }
        // A right password is an account's.
        let Some(user) = files.user(name) else {
            return (Answer::Refused(format!("there is no account {}", shown(name))), now);
        };
        let groups =
            accounts::other_groups(user, &files.groups, radix_abi::names::GROUPS_MAX as usize);
        let tty = signin::terminal_name(&terminal);
        // What happened while they were away comes first, on the
        // terminal, before the session's programs write to it.
        let notices = Notices::read();
        let theirs = notices.of(name);
        if !theirs.is_empty()
            && let Ok(copy) = terminal.try_clone()
        {
            let text = format!("\n{}\n\n", theirs.join("\n"));
            let _ = File::from(copy).write_all(text.as_bytes());
        }
        match signin::start(names, user, &groups, &terminal, term) {
            Ok(process) => {
                audit!(
                    "{name} signed in on {tty}: session process {process}, for process {} (user {})",
                    peer.process,
                    peer.user
                );
                if !theirs.is_empty()
                    && let Err(why) = notices.without(name).write()
                {
                    eprintln!("authd: {why}");
                }
                let process = process as i32;
                let session = Session { name: name.into(), terminal: tty, elevated: false };
                self.sessions.insert(process, session);
                self.clients[i].session = Some(process);
                (Answer::Done(process.to_string()), now)
            }
            Err(why) => (Answer::Refused(why), now),
        }
    }

    /// Takes in the sessions that ended, and tells the connections that
    /// wait for them.
    fn reap(&mut self, now: Instant) {
        let mut drained = [0u8; 64];
        while (&self.children).read(&mut drained).is_ok_and(|n| n > 0) {}
        loop {
            let mut status = 0;
            // SAFETY: waitpid writes the status.
            let process =
                unsafe { libc::waitpid(-1, &raw mut status, libc::WNOHANG | libc::WUNTRACED) };
            if process <= 0 {
                return;
            }
            // An elevated command stopped, as Ctrl+Z stops it: elevate
            // stops too, and asks for it to go on when it does.
            if libc::WIFSTOPPED(status) {
                if self.sessions.get(&process).is_some_and(|s| s.elevated) {
                    let line = request::stopped(libc::WSTOPSIG(status));
                    for client in self.clients.iter_mut().filter(|c| c.session == Some(process)) {
                        client.tell(line.as_bytes(), now);
                    }
                }
                continue;
            }
            if let Some(session) = self.sessions.remove(&process) {
                if session.elevated {
                    audit!(
                        "{}'s elevated process {process} on {} ended with status {status:#x}",
                        session.name,
                        session.terminal
                    );
                } else {
                    audit!(
                        "{} signed out on {}: session process {process} ended with status \
                         {status:#x}",
                        session.name,
                        session.terminal
                    );
                }
            }
            for client in &mut self.clients {
                if client.session == Some(process) {
                    client.session = None;
                    client.relay = None;
                    client.tell(request::ended(status).as_bytes(), now);
                    client.deadline = now + REQUEST_TIME;
                }
            }
        }
    }

    /// Checks the password of an elevate or unlock request from `peer`,
    /// `user`, in `pane`: none is needed in a pane unlocked, or from root;
    /// otherwise one must come, and be `user`'s. Whether one was typed, or
    /// the answer to give: `ok password` when one is needed and none came.
    /// When `ask` lets it and a program asks for passwords on the
    /// consoles, the pane's console is to ask for it instead
    /// ([`Authd::prompting`]).
    #[allow(clippy::too_many_arguments)]
    fn authorize(
        &mut self,
        files: &Files,
        user: &User,
        pane: Option<&Pane>,
        password: Option<&[u8]>,
        ask: bool,
        peer: Peer,
        now: Instant,
    ) -> Result<bool, (Answer, Instant)> {
        let open = pane.and_then(|p| self.unlocked.get(p)).is_some_and(|&until| until > now);
        if open || peer.is_root() {
            return Ok(false);
        }
        let Some(password) = password else {
            if ask
                && let Some(pane) = pane
                && self.has_prompter()
            {
                self.prompting = Some(Prompting {
                    terminal: pane.terminal.clone(),
                    name: user.name.clone(),
                    shown: String::new(),
                });
            }
            return Err((Answer::Done(request::NEEDS_PASSWORD.into()), now));
        };
        self.check_password(files, &user.name, password, peer, now)?;
        Ok(true)
    }

    /// Runs `asked` as an administrator for `peer`, client `i`, with the
    /// descriptors it sent: an administrator's to ask, with their
    /// password unless their pane is unlocked. The client then waits for
    /// the command's end, and passes signals on through its pipe.
    #[allow(clippy::too_many_arguments)]
    fn elevate(
        &mut self,
        i: usize,
        files: &Files,
        asked: &Elevation<'_>,
        password: Option<&[u8]>,
        ask: bool,
        peer: Peer,
        now: Instant,
    ) -> (Answer, Instant) {
        let mut descriptors = std::mem::take(&mut self.clients[i].descriptors);
        let pane = Pane::of(peer);
        let terminal = pane.as_ref().map_or_else(|| "no terminal".into(), |p| p.terminal.clone());
        let user = match administrator(files, peer) {
            Ok(user) => user.clone(),
            Err(why) => {
                audit!(
                    "{}, not an administrator, asked to elevate on {terminal}: {}, for process {}",
                    util::user_name(peer.user),
                    elevate::shown_command(asked.command),
                    peer.process
                );
                return (Answer::Refused(why), now);
            }
        };
        let root = elevate::root_account(files.users.entries().cloned());
        let plan: Plan = match elevate::plan(asked.mode, asked.command, asked.directory, &root) {
            Ok(plan) => plan,
            Err(why) => return (Answer::Refused(why), now),
        };
        if descriptors.len() != 4 {
            let why = "an elevate request comes with the command's three descriptors and a pipe";
            return (Answer::Refused(why.into()), now);
        }
        let typed = match self.authorize(files, &user, pane.as_ref(), password, ask, peer, now) {
            Ok(typed) => typed,
            Err(answer) => {
                // Asked on a console: the request keeps its descriptors
                // until the password comes.
                if let Some(prompting) = &mut self.prompting {
                    prompting.shown.clone_from(&plan.shown);
                    self.clients[i].descriptors = descriptors;
                }
                return answer;
            }
        };
        let relay = descriptors.pop();
        let env = elevate::environment(&root, &user, asked.term, asked.language, &plan.shown);
        let process = match elevate::start(&descriptors, &plan, &env) {
            Ok(process) => process as i32,
            Err(why) => return (Answer::Refused(why), now),
        };
        if let Some(pane) = pane {
            self.unlocked.insert(pane, now + elevate::UNLOCKED_FOR);
        }
        let how = if typed { "password typed" } else { "unlocked" };
        audit!(
            "{} elevated on {terminal} ({how}): process {process} runs {}, for process {}",
            user.name,
            plan.shown,
            peer.process
        );
        let session = Session { name: user.name, terminal, elevated: true };
        self.sessions.insert(process, session);
        let client = &mut self.clients[i];
        client.session = Some(process);
        client.relay = relay.map(|fd| {
            // SAFETY: making our own descriptor not wait.
            unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
            (fd, Vec::new())
        });
        (Answer::Done(process.to_string()), now)
    }

    /// Unlocks elevate in `peer`'s pane for 15 minutes, or 15 more.
    fn unlock(
        &mut self,
        files: &Files,
        password: Option<&[u8]>,
        ask: bool,
        peer: Peer,
        now: Instant,
    ) -> (Answer, Instant) {
        let user = match administrator(files, peer) {
            Ok(user) => user.clone(),
            Err(why) => return (Answer::Refused(why), now),
        };
        let Some(pane) = Pane::of(peer) else {
            let why = "elevate unlocks a terminal pane, and this program has no terminal";
            return (Answer::Refused(why.into()), now);
        };
        match self.authorize(files, &user, Some(&pane), password, ask, peer, now) {
            Ok(typed) => {
                if typed {
                    audit!(
                        "{} unlocked elevate on {} for 15 minutes, for process {}",
                        user.name,
                        pane.terminal,
                        peer.process
                    );
                }
                self.unlocked.insert(pane, now + elevate::UNLOCKED_FOR);
                (Answer::Done(String::new()), now)
            }
            Err(answer) => answer,
        }
    }

    /// Reads what elevate wrote to client `i`'s pipe, and passes on the
    /// signals it asks for to the command and those it started, its
    /// process group. The pipe's end is elevate's end: the command is
    /// then hung up.
    fn relay(&mut self, i: usize) {
        let client = &mut self.clients[i];
        let Some(process) = client.session else { return };
        let Some((fd, pending)) = &mut client.relay else { return };
        let mut buffer = [0u8; 256];
        let mut gone = false;
        loop {
            // SAFETY: reads into our buffer.
            let n = unsafe { libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
            if n > 0 {
                pending.extend_from_slice(&buffer[..n as usize]);
                // Nothing elevate writes is this long without a newline.
                if pending.len() > 1024 {
                    pending.clear();
                }
                continue;
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                _ if n == 0 => gone = true,
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => {}
                _ => gone = true,
            }
            break;
        }
        while let Some(end) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            match Relay::decode(&String::from_utf8_lossy(&line[..end])) {
                Some(Relay::Signal(signal)) => signal_command(process, signal),
                Some(Relay::Continue) => signal_command(process, libc::SIGCONT),
                None => {}
            }
        }
        if gone {
            client.relay = None;
            signal_command(process, libc::SIGHUP);
        }
    }

    /// Hangs up the elevated commands whose elevate went away before they
    /// ended, as a closed terminal would.
    fn hang_up_orphans(&self) {
        for client in self.clients.iter().filter(|c| c.done) {
            if let Some(process) = client.session
                && self.sessions.get(&process).is_some_and(|s| s.elevated)
            {
                signal_command(process, libc::SIGHUP);
            }
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

    /// Sets administrator `name`'s password to `password` from the
    /// startup-trouble menu (ADR 0253): root's to ask, only while the
    /// kernel was started for startup trouble. It is a security event, and
    /// every administrator, and `name`, is told at their next sign-in.
    fn reset(&mut self, files: &Files, name: &str, password: &[u8], peer: Peer) -> Answer {
        if !peer.is_root() {
            let why = "only the startup-trouble menu, which runs as root, resets a password";
            return Answer::Refused(why.into());
        }
        if !util::kernel_flag(STARTUP_TROUBLE) {
            let why = "a password is reset only from the startup-trouble menu: start the \
                       computer from its menu's startup-trouble entry";
            return Answer::Refused(why.into());
        }
        let Some(user) = files.user(name) else {
            return Answer::Refused(format!("there is no account {}", shown(name)));
        };
        // No built-in account has a password, root included, nor gets one.
        if !user.is_person() {
            return Answer::Refused(format!(
                "{name} is not a person's account and has no password"
            ));
        }
        if !accounts::is_admin(user, &files.groups) {
            return Answer::Refused(format!(
                "{name} is not an administrator: an administrator resets that password with \
                 elevate passwd {name}"
            ));
        }
        // The owner gets back in: a disabled account is enabled too.
        if let Err(why) = set_password(files, name, password, true) {
            return Answer::Refused(why);
        }
        // The new password opens the account at once.
        self.wrong.retain(|(account, _), _| account != name);
        let empty = if password.is_empty() { ", to an empty one" } else { "" };
        let enabled = if files.disabled(name) { ", and the account enabled," } else { "" };
        audit!(
            "the password of {name} was reset{empty}{enabled} from the startup-trouble menu, by \
             process {} (user {})",
            peer.process,
            peer.user
        );
        let when = util::local_time(util::now_ns() as i64 / 1_000_000_000, "%a %b %e %H:%M %Y");
        let mut notices = Notices::read();
        let admins =
            files.users.entries().filter(|u| u.is_person() && accounts::is_admin(u, &files.groups));
        for admin in admins {
            let notice = if admin.name == name {
                format!("Your password was reset from the startup-trouble menu on {when}.")
            } else {
                format!("The password of {name} was reset from the startup-trouble menu on {when}.")
            };
            notices.add(&admin.name, &notice);
        }
        if let Err(why) = notices.write() {
            eprintln!("authd: {why}");
        }
        Answer::Done(String::new())
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
        if let Err(why) = set_password(files, name, new, false) {
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

/// How many times [`Authd::end`] asks the kernel to end an account's
/// processes before it gives up: each time ends those that a process it
/// ended the time before made as it ended.
const END_PASSES: u32 = 100;

impl Authd {
    /// Ends everything that acts for `user` (ADR 0220): the elevated
    /// commands authd started for it, elevate unlocked in its panes, and
    /// every process the kernel has act for it, its sessions with them,
    /// while every capability made for it fails from then on. How many of
    /// its processes the kernel ended.
    fn end(&mut self, user: &User) -> Result<u32, String> {
        let Some(names) = self.names else {
            return Err("authd holds no names capability to end the account's processes with: \
                        its service file needs `names = true`"
                .into());
        };
        self.unlocked.retain(|pane, _| pane.user != user.uid);
        for (&process, _) in self.sessions.iter().filter(|(_, s)| s.elevated && s.name == user.name)
        {
            signal_command(process, libc::SIGKILL);
        }
        let mut ended = 0u32;
        for _ in 0..END_PASSES {
            let now = names::end(&sys::SYS, names, user.uid)
                .map_err(|status| {
                    format!("the kernel did not end its processes: {}", sys::status_name(status.0))
                })?
                .ended;
            ended = ended.saturating_add(now);
            if now == 0 {
                return Ok(ended);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Err(format!("its processes made more as fast as {ended} of them ended"))
    }

    /// Disables `name`'s account (ADR 0220): nothing signs in to it, and
    /// what acts for it ends.
    fn disable(&mut self, files: &Files, name: &str, peer: Peer) -> Answer {
        let user = match person(files, name, peer, "disables accounts") {
            Ok(user) => user.clone(),
            Err(why) => return Answer::Refused(why),
        };
        if let Err(why) = mark_disabled(files, name, true) {
            return Answer::Refused(why);
        }
        match self.end(&user) {
            Ok(ended) => {
                audit!(
                    "disabled the account {name} ({}), for process {} (user {}); {ended} of its \
                     processes ended",
                    user.uid,
                    peer.process,
                    peer.user
                );
                Answer::Done(ended.to_string())
            }
            Err(why) => {
                audit!("disabled the account {name} ({}), but {why}", user.uid);
                Answer::Refused(format!("{name} is disabled, but {why}"))
            }
        }
    }

    /// Removes `name`'s account: disables it, ends what acts for it,
    /// retires its number and takes its lines out of the account files.
    /// Its home is left to the asker.
    fn remove(&mut self, files: Files, name: &str, peer: Peer) -> Answer {
        let user = match person(&files, name, peer, "removes accounts") {
            Ok(user) => user.clone(),
            Err(why) => return Answer::Refused(why),
        };
        // Nothing signs in to it from now on, whatever fails later.
        if let Err(why) = mark_disabled(&files, name, true) {
            return Answer::Refused(why);
        }
        let ended = match self.end(&user) {
            Ok(ended) => ended,
            Err(why) => {
                audit!("disabled the account {name} ({}) to remove it, but {why}", user.uid);
                return Answer::Refused(format!("{name} is disabled but not removed: {why}"));
            }
        };
        let Files { mut users, mut groups, mut passwords } = files;
        if let Err(why) = accounts::remove(&mut users, &mut groups, name) {
            return Answer::Refused(why);
        }
        passwords.retain_entries(|p| p.name != name);
        // Its number first, so that it is never given again, then the
        // user, whose line going removes the account, and the rest.
        let day = util::local_time(util::now_ns() as i64 / 1_000_000_000, "%Y-%m-%d");
        let removed = retire(&user, &day)
            .and_then(|()| replace(accounts::PASSWD, &users.text(), 0o644))
            .and_then(|()| replace(accounts::GROUP, &groups.text(), 0o644))
            .and_then(|()| replace(accounts::PASSWORDS, &passwords.text(), 0o600));
        if let Err(why) = removed {
            return Answer::Refused(format!("{name} is disabled but not wholly removed: {why}"));
        }
        let notices = Notices::read();
        if !notices.of(name).is_empty()
            && let Err(why) = notices.without(name).write()
        {
            eprintln!("authd: {why}");
        }
        self.wrong.retain(|(account, _), _| account != name);
        audit!(
            "removed the account {name} ({}), for process {} (user {}); {ended} of its processes \
             ended, and its number is retired",
            user.uid,
            peer.process,
            peer.user
        );
        Answer::Done(user.home)
    }

    /// Renames `name`'s account to `new`, with its own group and its home
    /// in `/users`, while nothing acts for it.
    fn rename(&mut self, files: Files, name: &str, new: &str, peer: Peer) -> Answer {
        let user = match person(&files, name, peer, "renames accounts") {
            Ok(user) => user.clone(),
            Err(why) => return Answer::Refused(why),
        };
        // Its processes would go on with the old name and home.
        let running = match proc::processes() {
            Ok(all) => all.iter().filter(|p| p.user == user.uid && !p.zombie()).count(),
            Err(err) => return Answer::Refused(format!("cannot read /processes: {err}")),
        };
        if running > 0 {
            let processes =
                if running == 1 { "a process".to_string() } else { format!("{running} processes") };
            return Answer::Refused(format!(
                "{name} is running {processes}: rename the account once {name} has signed out, \
                 and what runs for them has ended"
            ));
        }
        let Files { mut users, mut groups, mut passwords } = files;
        let (before, after) = match accounts::rename(&mut users, &mut groups, name, new) {
            Ok(both) => both,
            Err(why) => return Answer::Refused(why),
        };
        for line in passwords.entries_mut().filter(|p| p.name == name) {
            line.name = new.into();
        }
        // Its home moves first, then the files say where it is.
        let moves = before.home != after.home && Path::new(&before.home).exists();
        if moves {
            if Path::new(&after.home).exists() {
                return Answer::Refused(format!("{} is there already", after.home));
            }
            if let Err(err) = fs::rename(&before.home, &after.home) {
                let err = util::strip_os_error(&err);
                return Answer::Refused(format!(
                    "cannot move {} to {}: {err}",
                    before.home, after.home
                ));
            }
        }
        let written = replace(accounts::PASSWORDS, &passwords.text(), 0o600)
            .and_then(|()| replace(accounts::GROUP, &groups.text(), 0o644))
            .and_then(|()| replace(accounts::PASSWD, &users.text(), 0o644));
        if let Err(why) = written {
            if moves {
                let _ = fs::rename(&after.home, &before.home);
            }
            return Answer::Refused(why);
        }
        let notices = Notices::read();
        if !notices.of(name).is_empty()
            && let Err(why) = notices.renamed(name, new).write()
        {
            eprintln!("authd: {why}");
        }
        self.wrong.retain(|(account, _), _| account != name);
        audit!(
            "renamed the account {name} ({}) to {new}, its home {}, for process {} (user {})",
            user.uid,
            after.home,
            peer.process,
            peer.user
        );
        Answer::Done(format!("renamed {name} to {new}, home {}", after.home))
    }

    /// Changes `name`'s full name, or whether it is an administrator.
    fn set(
        &mut self,
        files: Files,
        name: &str,
        full_name: Option<&str>,
        admin: Option<bool>,
        peer: Peer,
    ) -> Answer {
        let Some(user) = files.user(name).cloned() else {
            return Answer::Refused(format!("there is no account {}", shown(name)));
        };
        // Anyone changes their own full name, as chfn(1) lets them.
        let own = peer.user == user.uid && admin.is_none();
        if !peer.is_root() && !own {
            let why = "only an administrator, inside elevate, changes an account's settings; \
                       anyone changes their own full name";
            return Answer::Refused(why.into());
        }
        if !user.is_person() {
            return Answer::Refused(format!("{name} is not a person's account"));
        }
        let Files { mut users, mut groups, .. } = files;
        let mut changed = Vec::new();
        if let Some(full_name) = full_name {
            if let Err(why) = accounts::check_full_name(full_name) {
                return Answer::Refused(why);
            }
            for line in users.entries_mut().filter(|u| u.name == name) {
                line.full_name = full_name.into();
            }
            changed.push(format!("full name {full_name:?}"));
        }
        if let Some(admin) = admin {
            if let Err(why) = accounts::set_admin(&mut groups, name, admin) {
                return Answer::Refused(why);
            }
            if accounts::is_admin(&user, &groups) != admin {
                return Answer::Refused(format!(
                    "{name}'s own group is the {} group",
                    accounts::ADMINS
                ));
            }
            changed.push(if admin { "an administrator" } else { "not an administrator" }.into());
        }
        let written = replace(accounts::GROUP, &groups.text(), 0o644)
            .and_then(|()| replace(accounts::PASSWD, &users.text(), 0o644));
        if let Err(why) = written {
            return Answer::Refused(why);
        }
        // Someone who is no longer an administrator keeps nothing of the
        // admin set.
        if admin == Some(false) {
            self.unlocked.retain(|pane, _| pane.user != user.uid);
            for (&process, _) in self.sessions.iter().filter(|(_, s)| s.elevated && s.name == name)
            {
                signal_command(process, libc::SIGKILL);
            }
        }
        let changed = changed.join(", ");
        audit!(
            "changed the account {name} ({}): {changed}, for process {} (user {})",
            user.uid,
            peer.process,
            peer.user
        );
        Answer::Done(changed)
    }
}

/// The account of a person `name`, for a request that changes it: root's
/// to ask, from an administrator inside elevate, which `what` says.
fn person<'a>(files: &'a Files, name: &str, peer: Peer, what: &str) -> Result<&'a User, String> {
    if !peer.is_root() {
        return Err(format!("only an administrator, inside elevate, {what}"));
    }
    let user = files.user(name).ok_or_else(|| format!("there is no account {}", shown(name)))?;
    if !user.is_person() {
        return Err(format!("{} is not a person's account", shown(name)));
    }
    Ok(user)
}

/// Enables `name`'s account again, with the password it had.
fn enable(files: &Files, name: &str, peer: Peer) -> Answer {
    if let Err(why) = person(files, name, peer, "enables accounts") {
        return Answer::Refused(why);
    }
    if !files.disabled(name) {
        return Answer::Done(format!("{name} was not disabled"));
    }
    if let Err(why) = mark_disabled(files, name, false) {
        return Answer::Refused(why);
    }
    audit!("enabled the account {name}, for process {} (user {})", peer.process, peer.user);
    Answer::Done(String::new())
}

/// Marks `name`'s account disabled, or not, in the passwords file. One
/// with no line, which no password opens, gets `!*`, Linux's for an
/// account locked with no password, so that enabling it opens it to no
/// password still.
fn mark_disabled(files: &Files, name: &str, disabled: bool) -> Result<(), String> {
    if files.disabled(name) == disabled {
        return Ok(());
    }
    let mut passwords = files.passwords.clone();
    if let Some(line) = passwords.entries_mut().find(|p| p.name == name) {
        line.set_disabled(disabled);
    } else {
        passwords.push(Password { name: name.into(), hash: format!("{DISABLED}*") });
    }
    replace(accounts::PASSWORDS, &passwords.text(), 0o600)
}

/// Adds `user`'s number, removed on `day`, to the retired ones, which no
/// account gets again.
fn retire(user: &User, day: &str) -> Result<(), String> {
    let failed = |err: io::Error| {
        format!("cannot write {}: {}", accounts::RETIRED, util::strip_os_error(&err))
    };
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(accounts::RETIRED)
        .map_err(failed)?;
    file.write_all(accounts::retired_line(user, day).as_bytes()).map_err(failed)?;
    file.sync_all().map_err(failed)
}

/// Puts the hash of `password` in `name`'s line of the passwords file. A
/// disabled account stays disabled, unless `enable`.
fn set_password(files: &Files, name: &str, password: &[u8], enable: bool) -> Result<(), String> {
    let hash = hash_of(password)?;
    let mut passwords = files.passwords.clone();
    if let Some(line) = passwords.entries_mut().find(|p| p.name == name) {
        let disabled = line.is_disabled() && !enable;
        line.hash = hash;
        line.set_disabled(disabled);
    } else {
        passwords.push(Password { name: name.into(), hash });
    }
    replace(accounts::PASSWORDS, &passwords.text(), 0o600)
}

/// The kernel flag of the startup-trouble menu's boot entry
/// (dukovo-boot(7)).
pub const STARTUP_TROUBLE: &str = "startup-trouble";

/// Where the notices for people's next sign-in are kept.
const NOTICES: &str = "/system/state/authd/notices";

/// What each person is told at their next sign-in: a line `NAME<tab>TEXT`
/// each, in authd's own directory.
#[derive(Debug, Default, PartialEq, Eq)]
struct Notices(Vec<(String, String)>);

impl Notices {
    /// Those kept; none if there is no file, or it cannot be read.
    fn read() -> Notices {
        Notices::parse(&read_text(NOTICES).unwrap_or_default())
    }

    fn parse(text: &str) -> Notices {
        Notices(
            text.lines()
                .filter_map(|line| line.split_once('\t'))
                .map(|(name, text)| (name.to_string(), text.to_string()))
                .collect(),
        )
    }

    fn text(&self) -> String {
        self.0.iter().fold(String::new(), |mut all, (name, text)| {
            let _ = writeln!(all, "{name}\t{text}");
            all
        })
    }

    /// Adds one for `name`, kept to one line.
    fn add(&mut self, name: &str, text: &str) {
        let text = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
        self.0.push((name.to_string(), text));
    }

    /// `name`'s, oldest first.
    fn of(&self, name: &str) -> Vec<String> {
        self.0.iter().filter(|(n, _)| n == name).map(|(_, text)| text.clone()).collect()
    }

    /// All but `name`'s.
    fn without(&self, name: &str) -> Notices {
        Notices(self.0.iter().filter(|(n, _)| n != name).cloned().collect())
    }

    /// The same, `name`'s now `new`'s.
    fn renamed(&self, name: &str, new: &str) -> Notices {
        let rename = |n: &String| if n == name { new.to_string() } else { n.clone() };
        Notices(self.0.iter().map(|(n, text)| (rename(n), text.clone())).collect())
    }

    fn write(&self) -> Result<(), String> {
        replace(NOTICES, &self.text(), 0o600)
    }
}

/// What an `elevate` request asks to run.
struct Elevation<'a> {
    mode: Mode,
    term: &'a str,
    directory: &'a [u8],
    language: &'a [String],
    command: &'a [Vec<u8>],
}

/// The account of `peer`, if it is an administrator's, or why not.
fn administrator(files: &Files, peer: Peer) -> Result<&User, String> {
    let user = files
        .users
        .entries()
        .find(|u| u.uid == peer.user)
        .ok_or_else(|| format!("user {} has no account", peer.user))?;
    if !accounts::is_admin(user, &files.groups) {
        return Err(format!(
            "{} is not an administrator: only members of the {} group may elevate",
            user.name,
            accounts::ADMINS
        ));
    }
    Ok(user)
}

/// Sends `signal` to the elevated command `process` and those it
/// started, its process group, as a terminal does; to it alone if it
/// left its group.
fn signal_command(process: i32, signal: libc::c_int) {
    // SAFETY: kill only sends a signal, to authd's own child's group.
    unsafe {
        if libc::kill(-process, signal) != 0 {
            libc::kill(process, signal);
        }
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
    let retired = match read_text(accounts::RETIRED) {
        Ok(text) => accounts::retired(&text),
        Err(err) => return Answer::Refused(format!("cannot read {}: {err}", accounts::RETIRED)),
    };
    let user = match accounts::add(&mut users, &mut groups, name, full_name, admin, &retired) {
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
    /// Has it get `line` after what it is to get already, as soon as it
    /// can.
    fn tell(&mut self, line: &[u8], now: Instant) {
        match &mut self.answer {
            Some((bytes, _)) => bytes.extend_from_slice(line),
            None => self.answer = Some((line.to_vec(), now)),
        }
    }

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
        let mut sent = true;
        while !bytes.is_empty() {
            match self.stream.write(bytes) {
                Ok(0) => {
                    sent = false;
                    break;
                }
                Ok(n) => {
                    bytes.drain(..n);
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => {
                    sent = false;
                    break;
                }
            }
        }
        self.answer = None;
        // One whose session runs waits for its end, and consoled's
        // prompts connection for the next prompt; the rest are done.
        if sent && (self.session.is_some() || self.prompter) {
            self.deadline = now + SESSION_TIME;
        } else {
            self.done = true;
        }
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
            self.unlocked.retain(|_, until| *until > now);
            for client in &mut self.clients {
                if client.deadline <= now {
                    client.done = true;
                }
            }
            self.hang_up_orphans();
            self.tidy_prompts(now);
            self.clients.retain(|c| !c.done);
            let listening = self.clients.len() < CLIENTS_MAX;
            let mut fds = vec![
                libc::pollfd {
                    fd: if listening { self.listener.as_raw_fd() } else { -1 },
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd { fd: self.children.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            ];
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
            // Then the pipes of the elevated commands' elevates.
            let relays: Vec<usize> =
                (0..self.clients.len()).filter(|&i| self.clients[i].relay.is_some()).collect();
            for &i in &relays {
                let fd = self.clients[i].relay.as_ref().map_or(-1, |(fd, _)| fd.as_raw_fd());
                fds.push(libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
            }
            let next = self
                .clients
                .iter()
                .flat_map(|c| {
                    [
                        Some(c.deadline),
                        c.waiting.as_ref().map(|w| w.1),
                        c.answer.as_ref().map(|a| a.1),
                        c.asked.as_ref().map(|a| a.until),
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
            if fds[1].revents != 0 {
                self.reap(now);
            }
            let count = self.clients.len();
            for (i, entry) in fds[2..2 + count].iter().enumerate() {
                if entry.revents & libc::POLLOUT != 0 {
                    self.clients[i].send(now);
                }
                if entry.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                    self.read(i, now);
                }
            }
            for (&i, entry) in relays.iter().zip(&fds[2 + count..]) {
                if entry.revents != 0 {
                    self.relay(i);
                }
            }
            if fds[0].revents != 0 {
                self.accept(now);
            }
        }
    }
}

/// Has SIGCHLD write to a pipe, whose read end it returns, so that the
/// loop wakes when a session's first process or an elevated command ends
/// or stops.
fn watch_children() -> io::Result<File> {
    let mut fds = [0; 2];
    // SAFETY: pipe2 writes two descriptors.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    CHILDREN.store(fds[1], Ordering::Relaxed);
    // SAFETY: all zeros is a valid sigaction; the handler only writes to
    // the pipe; the read end is ours alone.
    unsafe {
        let mut action: libc::sigaction = util::zeroed();
        action.sa_sigaction = on_child as extern "C" fn(libc::c_int) as *const () as usize;
        // A stopped child wakes it too: an elevated command's elevate is
        // told.
        action.sa_flags = libc::SA_RESTART;
        libc::sigaction(libc::SIGCHLD, &raw const action, std::ptr::null_mut());
        Ok(File::from_raw_fd(fds[0]))
    }
}

/// Whether no account can be signed in to: no person's account, and
/// root's password locked, as on a machine that was never set up. Then
/// sign-in makes the owner's account first.
fn needs_owner(files: &Files) -> bool {
    let person = files.users.entries().any(User::is_person);
    let root_signs_in = !files.disabled("root")
        && files.hash("root").is_some_and(|phc| phc.is_empty() || hash::readable(phc));
    !person && !root_signs_in
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
    let names = signin::names_slot();
    if names.is_none() {
        eprintln!("authd: no names capability: sign-in cannot start sessions");
    }
    let children = watch_children().map_err(|e| util::failed(&"a pipe", e))?;
    eprintln!("authd: {} accounts; answering at {}", people.len(), accounts::SOCKET);
    let mut authd = Authd {
        listener,
        clients: Vec::new(),
        wrong: HashMap::new(),
        names,
        sessions: HashMap::new(),
        unlocked: HashMap::new(),
        next_prompt: 1,
        prompting: None,
        children,
    };
    authd.run_loop().map_err(|e| util::failed(&"authd", e))?;
    let _ = fs::remove_file(accounts::SOCKET);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_wait_for_each_person() {
        let mut notices = Notices::parse("alice\tYour password was reset.\nbad line\n");
        assert_eq!(notices.of("alice"), ["Your password was reset."]);
        notices.add("bob", "The password of alice\nwas reset.");
        notices.add("alice", "Another.");
        assert_eq!(notices.of("bob"), ["The password of alice was reset."]);
        assert_eq!(notices.of("alice").len(), 2);
        let left = notices.without("alice");
        assert_eq!(left.text(), "bob\tThe password of alice was reset.\n");
        assert_eq!(Notices::parse(&left.text()), left);
        assert!(notices.of("carol").is_empty());
        // A renamed account's go with it.
        let renamed = notices.renamed("alice", "ally");
        assert!(renamed.of("alice").is_empty());
        assert_eq!(renamed.of("ally").len(), 2);
        assert_eq!(renamed.of("bob"), ["The password of alice was reset."]);
    }

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
