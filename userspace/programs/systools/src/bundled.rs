// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! bundled(8): the bundle runtime, which installs bundles, seals them and
//! removes them for whoever asks (ADR 0223), links their commands where
//! the shell finds them, and tells bundle(1) which bundle to run and
//! what it recorded of it (ADR 0225).
//!
//! It answers on `/system/live/bundled/socket`, one request per
//! connection, each on a thread of its own, and tells who asks by the
//! user the kernel gives for the connection, never by what the request
//! says. It keeps what it installed in `/system/state/bundled`
//! (`bundles::registry`), which only it writes.
//!
//! A bundle is unpacked by the confined unpacker (ADR 0222) into a folder
//! bundled makes in the place's folder, checked again here, shown as the
//! install sheet, sealed (each file and folder root's and read-only), and
//! renamed to its ID. Every name in a person's folder is looked up
//! beneath a descriptor of that folder, never through a link, so nothing
//! there leads bundled elsewhere.
//!
//! Each place has a `commands` folder of bundled's, root's, which holds a
//! link by each command its bundles add, to bundle(1): `/applications/
//! commands` and each person's `applications/commands`, both on a
//! session's PATH. bundled makes it again from what it recorded after
//! each install and removal, and at its start.
//!
//! It keeps what each person said of each app's permissions, the grants
//! (ADR 0224, `bundles::grants`), in `/system/state/grants`, and asks them
//! through the trusted prompt: it sends authd(8) the question in the
//! system's own words, which consoled(8) shows on the console the
//! program runs on, where no program can draw or answer it. `bundle
//! grant` goes through the same prompt; taking a permission away needs
//! none, and reaches each run of the app at once, through the `watch`
//! connection its runner keeps.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use accounts::request::{Choice, Question};
use manifest::bundle::permissions::{self, Tier};
use manifest::bundle::{Manifest, Need, Request as Asked, RunsWith};

use crate::args::Args;
use crate::bundles::folder::{DEPTH_MAX, Dir, check, fd_stat, same};
use crate::bundles::grants::{self, Grant, Grants, Said};
use crate::bundles::minisign::PublicKey;
use crate::bundles::protocol::{self, Answer, Find, Found, LINE_MAX, Listed, Request};
use crate::bundles::registry::{self, Installed, Place, Registry};
use crate::bundles::sheet::{self, Facts};
use crate::bundles::{MANIFEST, confine, signed, sums};
use crate::util::{self, Fail};
use crate::{proc, signin, sys};

/// Where bundles for everyone are.
pub const EVERYONE: &str = "/applications";
/// A person's bundles' folder, in their home.
pub const APPLICATIONS: &str = "applications";
/// A person's apps' data folder, in their home.
pub const APP_DATA: &str = "app-data";
/// The folder of a place that holds a link by each command of its
/// bundles, which a session's PATH names.
pub const COMMANDS: &str = "commands";
/// Where each command's link leads: bundle(1), which runs the command of
/// that name (ADR 0225).
pub const LAUNCHER: &str = "/system/commands/bundle";
/// How long a request may take to come.
const REQUEST_TIME: Duration = Duration::from_secs(10);
/// How long the install sheet's question waits for its answer.
const ANSWER_TIME: Duration = Duration::from_mins(10);
/// The most connections answered at once.
const CONNECTIONS_MAX: usize = 16;
/// How long a permission's question waits for the person: authd's own
/// wait, and a little more.
const CHOICE_TIME: Duration = Duration::from_mins(6);
/// The most runs of apps one person's watching connections follow.
const WATCHERS_MAX: usize = 64;
/// The longest app name a question shows.
const NAME_SHOWN: usize = 40;

/// An audit event (ADR 0211): a security event of the log.
macro_rules! audit {
    ($($arg:tt)*) => { println!("<86>bundled: {}", format_args!($($arg)*)) };
}

/// Makes `fd`'s file root's, with `mode`.
fn set_root(fd: &OwnedFd, mode: libc::mode_t) -> io::Result<()> {
    // SAFETY: plain calls on a descriptor we hold.
    check(unsafe { libc::fchown(fd.as_raw_fd(), 0, 0) })?;
    // SAFETY: as above.
    check(unsafe { libc::fchmod(fd.as_raw_fd(), mode) })
}

/// Seals what is in `dir`: every folder root's and 0555, every file
/// root's and 0444, or 0555 if it may run. Anything but folders and
/// regular files is refused: the unpacker makes nothing else.
fn seal(dir: &Dir, depth: usize) -> Result<(), String> {
    if depth > DEPTH_MAX {
        return Err("its folders go too deep".into());
    }
    for name in dir.names().map_err(|e| format!("listing it: {e}"))? {
        let fd = dir.open_at(&name, false).map_err(|e| format!("{name}: {e}"))?;
        let st = fd_stat(&fd).map_err(|e| format!("{name}: {e}"))?;
        match st.st_mode & libc::S_IFMT {
            libc::S_IFDIR => {
                let inner = Dir(fd);
                seal(&inner, depth + 1).map_err(|why| format!("{name}/{why}"))?;
                set_root(&inner.0, 0o555).map_err(|e| format!("{name}: {e}"))?;
            }
            libc::S_IFREG => {
                // fchown and fchmod need a descriptor open for writing.
                let writable = dir.open_writable(&name).map_err(|e| format!("{name}: {e}"))?;
                let again = fd_stat(&writable).map_err(|e| format!("{name}: {e}"))?;
                if !same(&st, &again) {
                    return Err(format!("{name}: changed as it was sealed"));
                }
                let mode = if st.st_mode & 0o111 != 0 { 0o555 } else { 0o444 };
                set_root(&writable, mode).map_err(|e| format!("{name}: {e}"))?;
            }
            _ => return Err(format!("{name}: neither a file nor a folder")),
        }
    }
    Ok(())
}

/// What bundled shares between its threads.
struct Shared {
    registry: Mutex<Registry>,
    grants: Mutex<Grants>,
    /// The runs of apps that follow what is taken away from them.
    watchers: Mutex<Vec<Watcher>>,
    connections: AtomicUsize,
}

/// A run of an app, by its runner's `watch` connection.
struct Watcher {
    uid: u32,
    id: String,
    stream: UnixStream,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn grants(&self) -> MutexGuard<'_, Grants> {
        self.grants.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn watchers(&self) -> MutexGuard<'_, Vec<Watcher>> {
        self.watchers.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn state(name: &str) -> String {
    format!("{}/{name}", registry::STATE)
}

fn read_state(name: &str) -> io::Result<String> {
    match fs::read_to_string(state(name)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        other => other,
    }
}

/// Writes `text` to the file `path`, whole or not at all, with mode
/// `mode`.
fn write_whole(path: &str, text: &str, mode: u32) -> Result<(), String> {
    let new = format!("{path}.new");
    // One left behind would keep its mode.
    let _ = fs::remove_file(&new);
    let file = fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&new);
    let written = file.and_then(|mut f| {
        f.write_all(text.as_bytes())?;
        f.sync_all()
    });
    written.and_then(|()| fs::rename(&new, path)).map_err(|e| format!("{path}: {e}"))
}

/// Writes the registry's files, each whole or not at all.
fn save(registry: &Registry) -> Result<(), String> {
    let names = [registry::INSTALLED, registry::BOUND, registry::DEVELOPER_KEYS];
    for (name, text) in names.iter().zip(registry.texts()) {
        write_whole(&state(name), &text, 0o644)?;
    }
    Ok(())
}

/// Writes user `uid`'s grants, whole or not at all.
fn save_grants(grants: &Grants, uid: u32) -> Result<(), String> {
    let records = grants.0.get(&uid).map_or(&[][..], Vec::as_slice);
    write_whole(&format!("{}/{uid}", grants::FOLDER), &Grants::text(records), 0o600)
}

/// Reads every person's grants. A file bundled cannot read is set aside,
/// as `UID.bad`, and its person is asked again.
fn read_grants() -> Result<Grants, Fail> {
    let folder = grants::FOLDER;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(folder)
        .and_then(|()| fs::set_permissions(folder, fs::Permissions::from_mode(0o700)))
        .map_err(|e| util::failed(&folder, e))?;
    let mut grants = Grants::default();
    for entry in fs::read_dir(folder).map_err(|e| util::failed(&folder, e))?.flatten() {
        let name = entry.file_name();
        let Some(uid) = name.to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        let path = format!("{folder}/{uid}");
        let read = fs::read_to_string(&path).map_err(|e| e.to_string());
        match read.and_then(|text| Grants::read(&text)) {
            Ok(records) => {
                grants.0.insert(uid, records);
            }
            Err(why) => {
                eprintln!("bundled: {path}: {why}; set aside as {uid}.bad, and asked again");
                let _ = fs::rename(&path, format!("{folder}/{uid}.bad"));
            }
        }
    }
    Ok(grants)
}

/// Now, in seconds since 1970.
fn now() -> u64 {
    util::now_ns() / 1_000_000_000
}

/// The user at the other end of `stream`, as the kernel gives it.
fn asker(stream: &UnixStream) -> Option<u32> {
    let slot = sys::slot_of(stream.as_raw_fd());
    radix_abi::file::socket_state(&sys::SYS, slot).ok().map(|state| state.user)
}

/// The terminal of the process at the other end of `stream`, as
/// /processes names it, if it acts for `uid`: where to ask its person.
fn terminal_of(stream: &UnixStream, uid: u32) -> Option<String> {
    let slot = sys::slot_of(stream.as_raw_fd());
    let state = radix_abi::file::socket_state(&sys::SYS, slot).ok()?;
    let process = proc::process(state.peer).ok()?;
    if process.user != uid {
        return None;
    }
    process.terminal.filter(|t| t != "none")
}

/// The account of user `uid`.
pub(crate) fn account(uid: u32) -> Result<accounts::User, String> {
    let text =
        fs::read_to_string(accounts::PASSWD).map_err(|e| format!("{}: {e}", accounts::PASSWD))?;
    accounts::users(&text)
        .entries()
        .find(|u| u.uid == uid)
        .cloned()
        .ok_or_else(|| format!("user {uid} has no account"))
}

/// The folder of `place`, as a path, and the user who must own it.
pub(crate) fn place_folder(place: Place) -> Result<(String, u32), String> {
    match place {
        Place::Everyone => Ok((EVERYONE.into(), 0)),
        Place::User(uid) => {
            let user = account(uid)?;
            Ok((format!("{}/{APPLICATIONS}", user.home.trim_end_matches('/')), uid))
        }
    }
}

/// Opens the folder of `place`, checking that it is a folder of its
/// owner's, not a link.
fn open_place(place: Place) -> Result<(Dir, String), String> {
    let (path, owner) = place_folder(place)?;
    let dir = Dir::open(&path).map_err(|e| format!("{path}: {e}"))?;
    let st = dir.stat().map_err(|e| format!("{path}: {e}"))?;
    if st.st_uid != owner {
        return Err(format!("{path} is not its owner's: bundled installs nothing there"));
    }
    Ok((dir, path))
}

fn whom(place: Place) -> String {
    match place {
        Place::Everyone => "everyone".into(),
        Place::User(uid) => account(uid).map_or_else(|_| format!("user {uid}"), |u| u.name),
    }
}

/// A connection, which answers lines.
struct Connection {
    stream: UnixStream,
    input: BufReader<Timed>,
}

/// The asker's side of a connection, each read of which waits no later
/// than `deadline`: a local socket takes no read timeout here, so poll(2)
/// keeps a client that sends a part of a line, or nothing, from holding
/// a thread.
struct Timed {
    stream: UnixStream,
    deadline: Instant,
}

impl Read for Timed {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        readable(&self.stream, self.deadline)?;
        self.stream.read(buf)
    }
}

impl Connection {
    fn send(&mut self, answer: &Answer) -> io::Result<()> {
        self.stream.write_all(&answer.bytes())
    }

    /// The next line the asker sends, without its newline.
    fn line(&mut self, time: Duration) -> io::Result<String> {
        self.input.get_mut().deadline = Instant::now() + time;
        let mut line = Vec::new();
        self.input.by_ref().take(LINE_MAX as u64 + 1).read_until(b'\n', &mut line)?;
        if line.pop() != Some(b'\n') {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "no line"));
        }
        String::from_utf8(line).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not text"))
    }
}

/// Waits until `stream` has something to read, or `deadline` passes.
fn readable(stream: &UnixStream, deadline: Instant) -> io::Result<()> {
    let mut wanted = libc::pollfd { fd: stream.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let ms = i32::try_from(left.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: one pollfd, valid for the call.
        let n = unsafe { libc::poll(&raw mut wanted, 1, ms) };
        if n > 0 {
            return Ok(());
        }
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// A name nobody else uses for a folder of bundled's in a place.
fn working_name(what: &str) -> Result<String, String> {
    let mut bytes = [0u8; 8];
    File::open(crate::bundles::RANDOM)
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| format!("random bytes: {e}"))?;
    Ok(format!(".{what}-{:016x}", u64::from_le_bytes(bytes)))
}

/// Whether `name` is one of bundled's working folders.
fn is_working(name: &str) -> bool {
    name.starts_with(".installing-") || name.starts_with(".removing-")
}

/// Removes the working folders bundled left in `dir` when it stopped,
/// those that are root's.
fn tidy(dir: &Dir) {
    for name in dir.names().unwrap_or_default().into_iter().filter(|n| is_working(n)) {
        if let Ok(Some(st)) = dir.stat_at(&name)
            && st.st_uid == 0
        {
            let _ = dir.remove_tree(&name, 0);
        }
    }
}

fn install(
    shared: &Shared,
    connection: &mut Connection,
    asker: u32,
    archive: &File,
    everyone: bool,
    yes: bool,
) -> Result<String, String> {
    if everyone && asker != 0 {
        return Err("installing for everyone is an administrator's: run elevate bundle install \
                    --for-everyone"
            .into());
    }
    let place = if everyone { Place::Everyone } else { Place::User(asker) };
    let (places, path) = open_place(place)?;
    tidy(&places);
    let working = working_name("installing")?;
    places.make(&working, 0o700).map_err(|e| format!("{path}/{working}: {e}"))?;
    let result =
        install_in(shared, connection, asker, archive, place, &places, &path, &working, yes);
    if result.is_err() {
        let _ = places.remove_tree(&working, 0);
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn install_in(
    shared: &Shared,
    connection: &mut Connection,
    asker: u32,
    archive: &File,
    place: Place,
    places: &Dir,
    path: &str,
    working: &str,
    yes: bool,
) -> Result<String, String> {
    let folder = places.dir_at(working).map_err(|e| format!("{path}/{working}: {e}"))?;
    let made = folder.stat().map_err(|e| format!("{path}/{working}: {e}"))?;
    if made.st_uid != 0 || made.st_mode & 0o7777 != 0o700 {
        return Err(format!("{path}/{working} changed as it was made"));
    }
    let report = confine::unpack_into(archive, &folder.0)?;
    // Checked again here, as the unpacker read what came from anywhere:
    // the signature, the list and every byte, and nothing else there.
    let unpacked = format!("{path}/{working}");
    let still = |what: &str| -> Result<(), String> {
        match places.stat_at(working) {
            Ok(Some(st)) if same(&st, &made) => Ok(()),
            _ => Err(format!("{unpacked} changed {what}")),
        }
    };
    still("as it was unpacked")?;
    let checked = signed::check_folder(Path::new(&unpacked))?;
    still("as it was checked")?;
    let id = checked.manifest.id.0.clone();
    if checked.key != report.key || id != report.id || checked.manifest.version.0 != report.version
    {
        return Err("the unpacker's report is not what it unpacked".into());
    }
    let list = fs::read(Path::new(&unpacked).join(sums::LIST)).map_err(|e| format!("{e}"))?;
    let list = sums::hex(&sums::sha256(&list));
    let (trust, others) = {
        let registry = shared.lock();
        let trust = registry.trust(place, asker, &id, &checked.key)?;
        (trust, commands_in_reach(&registry, place, &id))
    };
    for command in &checked.manifest.commands {
        let name = command.name.to_string();
        if Path::new("/system/commands").join(&name).exists() {
            return Err(format!("its command {name} is one of the system's"));
        }
        if let Some(other) = others.iter().find(|(c, _)| *c == name) {
            return Err(format!("its command {name} is one {} already adds", other.1));
        }
    }
    let facts = Facts {
        manifest: &checked.manifest,
        key: &checked.key,
        trust: &trust,
        place,
        files: checked.files,
        bytes: checked.bytes,
    };
    connection.send(&Answer::Sheet(sheet::write(&facts))).map_err(|e| format!("{e}"))?;
    if !yes {
        connection.send(&Answer::Question).map_err(|e| format!("{e}"))?;
        match connection.line(ANSWER_TIME).as_deref() {
            Ok("yes") => {}
            Ok(_) => return Err("not installed, as you said".into()),
            Err(_) => return Err("not installed: no answer came".into()),
        }
    }
    seal(&folder, 0)?;
    set_root(&folder.0, 0o555).map_err(|e| format!("{unpacked}: {e}"))?;
    still("as it was sealed")?;
    let version = checked.manifest.version.0.clone();
    let mut registry = shared.lock();
    // Bound since? Another install may have come in between.
    let trust = registry.trust(place, asker, &id, &checked.key)?;
    let replaced = match places.stat_at(&id).map_err(|e| format!("{path}/{id}: {e}"))? {
        Some(_) => {
            let old = working_name("removing")?;
            places.rename(&id, &old).map_err(|e| format!("{path}/{id}: {e}"))?;
            Some(old)
        }
        None => None,
    };
    places.rename(working, &id).map_err(|e| format!("{path}/{id}: {e}"))?;
    if let Some(old) = &replaced {
        let _ = places.remove_tree(old, 0);
    }
    match places.stat_at(&id) {
        Ok(Some(st)) if same(&st, &made) => {}
        _ => return Err(format!("{path}/{id} changed as it was installed")),
    }
    let when = util::now_ns() / 1_000_000_000;
    let previous = registry.installed(place, &id).map(|i| i.version.clone());
    registry.record(Installed {
        place,
        id: id.clone(),
        version: version.clone(),
        key: checked.key,
        list,
        when,
    });
    save(&registry)?;
    drop(registry);
    let why = match trust {
        registry::Trust::Developer => "their developer key",
        registry::Trust::Bound { .. } | registry::Trust::BoundForEveryone { .. } => "its key",
        registry::Trust::Unknown => "a key this computer did not know",
    };
    let asked_by = whom(Place::User(asker));
    let over = previous.map_or_default(|v| format!(" in place of {v}"));
    audit!(
        "{asked_by} installed {id} {version} for {}{over}, signed by key {} ({why})",
        whom(place),
        checked.key.encoded()
    );
    let whose = if place == Place::Everyone { "everyone" } else { "you" };
    let linked = link_commands(&shared.lock(), place)
        .err()
        .map_or_default(|why| format!("; its commands are not linked: {why}"));
    Ok(format!("installed {id} {version}{over}, for {whose}{linked}"))
}

/// The commands of the bundles in reach of `place` but `id`, and whose.
fn commands_in_reach(registry: &Registry, place: Place, id: &str) -> Vec<(String, String)> {
    let mut commands = Vec::new();
    for installed in &registry.installed {
        let reach = installed.place == place || installed.place == Place::Everyone;
        if !reach || installed.id == id {
            continue;
        }
        if let Ok(manifest) = installed_manifest(installed.place, &installed.id) {
            for command in &manifest.commands {
                commands.push((command.name.to_string(), installed.id.clone()));
            }
        }
    }
    commands
}

/// The commands of the bundles installed in `place`.
fn commands_of(registry: &Registry, place: Place) -> Vec<String> {
    let mut commands: Vec<String> = registry
        .installed
        .iter()
        .filter(|i| i.place == place)
        .filter_map(|i| installed_manifest(i.place, &i.id).ok())
        .flat_map(|m| m.commands.into_iter().map(|c| c.name.to_string()))
        .collect();
    commands.sort();
    commands.dedup();
    commands
}

/// Makes the `commands` folder of `place` hold a link to [`LAUNCHER`] by
/// each command of its bundles, and nothing else: root's, mode 0755, made
/// if it is not there. A `commands` there that is not a folder of root's
/// is not bundled's, and bundled links nothing in it.
fn link_commands(registry: &Registry, place: Place) -> Result<(), String> {
    let (places, path) = open_place(place)?;
    let folder = format!("{path}/{COMMANDS}");
    match places.stat_at(COMMANDS).map_err(|e| format!("{folder}: {e}"))? {
        None => {
            places.make(COMMANDS, 0o755).map_err(|e| format!("{folder}: {e}"))?;
            let made = places.dir_at(COMMANDS).map_err(|e| format!("{folder}: {e}"))?;
            set_root(&made.0, 0o755).map_err(|e| format!("{folder}: {e}"))?;
        }
        Some(st) if st.st_mode & libc::S_IFMT == libc::S_IFDIR && st.st_uid == 0 => {}
        Some(_) => return Err(format!("{folder} is not bundled's, so it links nothing there")),
    }
    let links = places.dir_at(COMMANDS).map_err(|e| format!("{folder}: {e}"))?;
    let st = links.stat().map_err(|e| format!("{folder}: {e}"))?;
    if st.st_uid != 0 {
        return Err(format!("{folder} changed as it was opened"));
    }
    let wanted = commands_of(registry, place);
    for name in links.names().map_err(|e| format!("{folder}: {e}"))? {
        let target = links.read_link(&name).map_err(|e| format!("{folder}/{name}: {e}"))?;
        let keep = wanted.contains(&name) && target.as_deref() == Some(LAUNCHER);
        if target.is_some() && !keep {
            links.unlink(&name).map_err(|e| format!("{folder}/{name}: {e}"))?;
        }
    }
    for name in &wanted {
        if links.stat_at(name).map_err(|e| format!("{folder}/{name}: {e}"))?.is_none() {
            links.symlink(LAUNCHER, name).map_err(|e| format!("{folder}/{name}: {e}"))?;
        }
    }
    Ok(())
}

/// The bundle to run that `find` names, the asker's own before
/// everyone's, and what bundled recorded of it.
fn find(
    shared: &Shared,
    connection: &mut Connection,
    asker: u32,
    find: &Find,
) -> Result<String, String> {
    let found = {
        let registry = shared.lock();
        let places = [Place::User(asker), Place::Everyone];
        let mut found = None;
        for place in places {
            found = registry.installed.iter().filter(|i| i.place == place).find(|i| match find {
                Find::Id(id) => i.id == *id,
                Find::Command(name) => installed_manifest(i.place, &i.id)
                    .is_ok_and(|m| m.commands.iter().any(|c| c.name == *name)),
            });
            if found.is_some() {
                break;
            }
        }
        found.cloned()
    };
    let Some(installed) = found else {
        return Err(match find {
            Find::Id(id) => format!("{id} is not installed"),
            Find::Command(name) => format!("no bundle installed adds the command {name}"),
        });
    };
    let answer = Answer::Found(Found {
        place: installed.place,
        id: installed.id.clone(),
        key: installed.key,
        list: installed.list.clone(),
    });
    connection.send(&answer).map_err(|e| format!("{e}"))?;
    send_grants(shared, connection, asker, &installed.id, &installed.key)?;
    Ok(format!("{} {}", installed.id, installed.version))
}

/// Sends what `asker` said of app `id`'s permissions, signed by `key`.
fn send_grants(
    shared: &Shared,
    connection: &mut Connection,
    asker: u32,
    id: &str,
    key: &PublicKey,
) -> Result<(), String> {
    let said = shared.grants().of(asker, id, key);
    for grant in said {
        connection.send(&Answer::Granted(grant)).map_err(|e| format!("{e}"))?;
    }
    Ok(())
}

/// The manifest of the bundle `id` installed in `place`.
fn installed_manifest(place: Place, id: &str) -> Result<manifest::bundle::Manifest, String> {
    let (path, _) = place_folder(place)?;
    let bytes =
        fs::read(format!("{path}/{id}/{MANIFEST}")).map_err(|e| format!("{path}/{id}: {e}"))?;
    signed::parse_manifest(&bytes)
}

fn list(shared: &Shared, connection: &mut Connection, asker: u32) -> Result<String, String> {
    let installed: Vec<Installed> = shared
        .lock()
        .installed
        .iter()
        .filter(|i| i.place == Place::Everyone || i.place == Place::User(asker))
        .cloned()
        .collect();
    for i in &installed {
        let name = installed_manifest(i.place, &i.id).map_or_else(|_| "?".into(), |m| m.name.0);
        let listed = Listed {
            place: i.place,
            id: i.id.clone(),
            version: i.version.clone(),
            key: i.key,
            when: i.when,
            name: sheet::shown(&name, 60),
        };
        connection.send(&Answer::Listed(listed)).map_err(|e| format!("{e}"))?;
    }
    Ok(format!("{} bundles", installed.len()))
}

fn info(
    shared: &Shared,
    connection: &mut Connection,
    asker: u32,
    id: &str,
) -> Result<String, String> {
    let (installed, bound) = {
        let registry = shared.lock();
        let own = registry.installed(Place::User(asker), id);
        let found = own.or_else(|| registry.installed(Place::Everyone, id)).cloned();
        let found = found.ok_or_else(|| format!("{id} is not installed"))?;
        let bound = registry.bound(found.place, id).map_or(found.when, |b| b.since);
        (found, bound)
    };
    let (path, _) = place_folder(installed.place)?;
    let folder = format!("{path}/{id}");
    let manifest = installed_manifest(installed.place, id)?;
    let list =
        fs::read_to_string(format!("{folder}/{}", sums::LIST)).map_err(|e| format!("{e}"))?;
    let listed = sums::read(&list)?;
    let bytes: u64 = listed
        .iter()
        .filter_map(|e| fs::metadata(format!("{folder}/{}", e.path)).ok())
        .map(|m| m.len())
        .sum();
    let trust = registry::Trust::Bound { since: bound };
    let facts = Facts {
        manifest: &manifest,
        key: &installed.key,
        trust: &trust,
        place: installed.place,
        files: listed.len(),
        bytes,
    };
    connection.send(&Answer::Sheet(sheet::write(&facts))).map_err(|e| format!("{e}"))?;
    send_grants(shared, connection, asker, id, &installed.key)?;
    let data = match installed.place {
        Place::Everyone => format!("each person's {APP_DATA}/{id}"),
        Place::User(_) => account(asker)
            .map_or_else(|_| format!("{APP_DATA}/{id}"), |u| format!("{}/{APP_DATA}/{id}", u.home)),
    };
    let when =
        util::time_text(i64::try_from(installed.when).unwrap_or(0), "%Y-%m-%d %H:%M UTC", true);
    Ok(format!("installed on {when}; in {folder}; its data: {data}"))
}

fn remove(
    shared: &Shared,
    asker: u32,
    id: &str,
    everyone: bool,
    purge: bool,
) -> Result<String, String> {
    if everyone && asker != 0 {
        return Err("removing for everyone is an administrator's: run elevate bundle remove \
                    --for-everyone"
            .into());
    }
    let place = if everyone { Place::Everyone } else { Place::User(asker) };
    let mut registry = shared.lock();
    let Some(installed) = registry.installed(place, id).cloned() else {
        if !everyone && registry.installed(Place::Everyone, id).is_some() {
            return Err(format!(
                "{id} is installed for everyone: an administrator removes it with elevate bundle \
                 remove --for-everyone {id}"
            ));
        }
        return Err(format!("{id} is not installed"));
    };
    let (places, path) = open_place(place)?;
    if places.stat_at(id).map_err(|e| format!("{path}/{id}: {e}"))?.is_some() {
        let old = working_name("removing")?;
        places.rename(id, &old).map_err(|e| format!("{path}/{id}: {e}"))?;
        places.remove_tree(&old, 0).map_err(|e| format!("{path}/{old}: {e}"))?;
    }
    registry.forget(place, id);
    save(&registry)?;
    let _ = link_commands(&registry, place);
    drop(registry);
    let mut data = String::new();
    let mut purged = false;
    if purge && let Place::User(uid) = place {
        let user = account(uid)?;
        let folder = format!("{}/{APP_DATA}", user.home.trim_end_matches('/'));
        let dir = Dir::open(&folder).map_err(|e| format!("{folder}: {e}"))?;
        if dir.stat().is_ok_and(|st| st.st_uid == uid) {
            dir.remove_tree(id, 0).map_err(|e| format!("{folder}/{id}: {e}"))?;
            data = format!(", and its data in {folder}/{id}");
            purged = true;
        }
        // What they said of it goes with its data.
        let mut grants = shared.grants();
        if grants.forget(uid, id) {
            save_grants(&grants, uid)?;
        }
    }
    audit!(
        "{} removed {id} {} for {}{data}",
        whom(Place::User(asker)),
        installed.version,
        whom(place)
    );
    let and = if purged { ", and its data" } else { "" };
    Ok(format!("removed {id} {}{and}", installed.version))
}

/// The bundle `id` that `asker` runs: their own before everyone's.
fn installed_for(shared: &Shared, asker: u32, id: &str) -> Result<Installed, String> {
    let registry = shared.lock();
    let own = registry.installed(Place::User(asker), id);
    own.or_else(|| registry.installed(Place::Everyone, id))
        .cloned()
        .ok_or_else(|| format!("{id} is not installed"))
}

/// What `manifest` asks of `permission`: as it names it; for
/// runs-with-your-access, also as its commands that run with your access
/// ask for it.
fn asked_of(manifest: &Manifest, permission: &str) -> Option<Asked> {
    let named = manifest.permissions.iter().find(|(name, _)| *name == permission);
    if let Some((_, request)) = named {
        return Some(request.clone());
    }
    let yours = manifest.commands.iter().any(|c| c.runs_with == RunsWith::YourAccess);
    (permission == grants::YOUR_ACCESS && yours).then(|| Asked {
        need: Need::Required,
        why: None,
        folders: Vec::new(),
        ports: Vec::new(),
        apps: Vec::new(),
    })
}

/// Asks `name`, on `terminal`, the question `text`, through authd(8) and
/// the console's trusted prompt: what they chose ([`Choice::word`]),
/// [`accounts::request::CANCELLED`] or [`accounts::request::ELSEWHERE`].
fn ask_person(
    terminal: &str,
    name: &str,
    question: Question,
    text: &str,
) -> Result<String, String> {
    let stream = UnixStream::connect(accounts::SOCKET).map_err(|e| {
        format!("cannot reach authd at {}: {}", accounts::SOCKET, util::strip_os_error(&e))
    })?;
    let request = accounts::request::Request::Choose {
        terminal: terminal.into(),
        name: name.into(),
        question,
        text: text.into(),
    };
    (&stream)
        .write_all(&request.encode())
        .and_then(|()| stream.shutdown(Shutdown::Write))
        .map_err(|e| format!("cannot ask authd: {e}"))?;
    let mut input = BufReader::new(Timed { stream, deadline: Instant::now() + CHOICE_TIME });
    let mut line = String::new();
    input
        .by_ref()
        .take(LINE_MAX as u64)
        .read_line(&mut line)
        .map_err(|e| format!("authd did not answer: {e}"))?;
    match accounts::request::Answer::decode(&line) {
        accounts::request::Answer::Done(word) => Ok(word),
        accounts::request::Answer::Refused(why) => Err(why),
    }
}

/// Keeps what `uid` said of app `id`, signed by `key`, and writes it.
fn record(shared: &Shared, uid: u32, id: &str, key: PublicKey, grant: Grant) -> Result<(), String> {
    let mut grants = shared.grants();
    grants.set(uid, id, key, grant)?;
    save_grants(&grants, uid)
}

/// The words of a grant, for the audit log.
fn shown_words(words: &[String]) -> String {
    if words.is_empty() { String::new() } else { format!(" ({})", words.join(", ")) }
}

/// The question an app's permission is asked with: a question's words
/// begin with the app's name and ID, as the manifest says them.
fn named(manifest: &Manifest) -> String {
    format!("{} ({})", sheet::shown(&manifest.name.0, NAME_SHOWN), manifest.id)
}

/// `ask`: asks the person whether app `id` may have `permission`, as its
/// manifest asks for it, the first time it runs wanting it (ADR 0224).
fn ask(
    shared: &Shared,
    connection: &Connection,
    asker: u32,
    id: &str,
    permission: &str,
    list: &str,
) -> Result<String, String> {
    let installed = installed_for(shared, asker, id)?;
    if installed.list != list {
        return Err(format!("{id} changed as it started: run it again"));
    }
    let manifest = installed_manifest(installed.place, id)?;
    let found = permissions::find(permission).ok_or("no such permission")?;
    let request = asked_of(&manifest, permission)
        .ok_or_else(|| format!("{id} does not ask for {permission}"))?;
    if !matches!(found.tier, Tier::FirstUse | Tier::Warning) {
        return Err(format!("{permission} is not asked: it is given {}", found.tier.words()));
    }
    grants::givable(found).map_err(|why| format!("{permission}, {why}"))?;
    let asked = grants::words(&request);
    // Another run may have asked meanwhile: what it was answered holds,
    // if it was all of this or none of it.
    let before = shared.grants().of(asker, id, &installed.key);
    let held =
        before.iter().find(|g| g.permission == permission && grants::covers(&g.asked, &asked));
    match held {
        Some(g) if g.said == Said::Never => return Ok(Choice::Never.word().into()),
        Some(g) if grants::covers(&g.given, &asked) => return Ok(Choice::Always.word().into()),
        _ => {}
    }
    let Some(terminal) = terminal_of(&connection.stream, asker) else {
        return Ok(accounts::request::ELSEWHERE.into());
    };
    let user = account(asker)?;
    let question =
        if found.tier == Tier::Warning { Question::Warning } else { Question::Permission };
    let text =
        format!("{} asks to: {}", named(&manifest), grants::phrase(found, Some(&request), &asked));
    let word = ask_person(&terminal, &user.name, question, &text)?;
    let Some(choice) = Choice::from_word(&word) else { return Ok(word) };
    let words = shown_words(&asked);
    let said = match choice {
        Choice::Once => {
            audit!("{} allowed {id} {permission}{words} once, on {terminal}", user.name);
            return Ok(word);
        }
        Choice::Always => Said::Allow,
        Choice::Never => Said::Never,
    };
    let given = if said == Said::Allow { asked.clone() } else { Vec::new() };
    let grant = Grant { permission: permission.into(), said, since: now(), given, asked };
    record(shared, asker, id, installed.key, grant)?;
    let how = if said == Said::Allow { "allowed" } else { "did not allow" };
    audit!("{} {how} {id} {permission}{words}, from now on, on {terminal}", user.name);
    Ok(word)
}

/// `bundle grant`: gives app `id` `permission`, with `words` or as it
/// asks for it, once the person says yes at the trusted prompt, so that
/// no program can give itself anything by running bundle.
fn grant(
    shared: &Shared,
    connection: &Connection,
    asker: u32,
    id: &str,
    permission: &str,
    words: Vec<String>,
) -> Result<String, String> {
    let installed = installed_for(shared, asker, id)?;
    let manifest = installed_manifest(installed.place, id)?;
    let found = permissions::find(permission).ok_or("no such permission")?;
    grants::givable(found).map_err(|why| format!("{permission}, {why}"))?;
    let request = asked_of(&manifest, permission)
        .ok_or_else(|| format!("{id} does not ask for {permission}: there is nothing to give"))?;
    let asked = grants::words(&request);
    if !grants::covers(&asked, &words) {
        return Err(format!(
            "{id} asks for no more than {}: give that, or less",
            if asked.is_empty() { "the permission".into() } else { asked.join(" ") }
        ));
    }
    let mut given = if words.is_empty() { asked.clone() } else { words };
    given.sort();
    given.dedup();
    let folders: Vec<&str> =
        given.iter().filter_map(|w| w.split_once('=')).map(|(name, _)| name).collect();
    if let Some(twice) = folders.iter().enumerate().find(|(i, f)| folders[..*i].contains(f)) {
        return Err(format!("{} is named twice: give it to read, or to read and change", twice.1));
    }
    let elsewhere = "bundle grant asks you on a console, where no program can answer for you, \
                     and this terminal is not one";
    let terminal = terminal_of(&connection.stream, asker).ok_or(elsewhere)?;
    let user = account(asker)?;
    let text =
        format!("Give {}: {}", named(&manifest), grants::phrase(found, Some(&request), &given));
    let word = ask_person(&terminal, &user.name, Question::Grant, &text)?;
    match Choice::from_word(&word) {
        Some(Choice::Always) => {}
        Some(_) => return Err("not given: you said no".into()),
        None if word == accounts::request::CANCELLED => {
            return Err("not given: you closed the question".into());
        }
        None => return Err(elsewhere.into()),
    }
    let shown = shown_words(&given);
    let grant =
        Grant { permission: permission.into(), said: Said::Allow, since: now(), given, asked };
    record(shared, asker, id, installed.key, grant)?;
    audit!("{} gave {id} {permission}{shown}, on {terminal}", user.name);
    Ok(format!("gave {id} {permission}{shown}"))
}

/// `bundle revoke`: takes `permission` away from app `id`, now and from
/// now on: each run of it that watches is told at once. Taking away needs
/// no prompt.
fn revoke(shared: &Shared, asker: u32, id: &str, permission: &str) -> Result<String, String> {
    let installed = installed_for(shared, asker, id)?;
    let manifest = installed_manifest(installed.place, id)?;
    let found = permissions::find(permission).ok_or("no such permission")?;
    let request = asked_of(&manifest, permission)
        .ok_or_else(|| format!("{id} does not ask for {permission}"))?;
    if found.tier == Tier::Admin {
        return Err(format!("{permission} is an administrator's, and no app has it yet"));
    }
    let asked = grants::words(&request);
    let grant = Grant {
        permission: permission.into(),
        said: Said::Never,
        since: now(),
        given: Vec::new(),
        asked,
    };
    record(shared, asker, id, installed.key, grant)?;
    let user = account(asker).map_or_else(|_| format!("user {asker}"), |u| u.name);
    audit!("{user} took {permission} away from {id}");
    let told = tell_watchers(shared, asker, id, permission);
    let running = if told > 0 { format!("; {told} running told at once") } else { String::new() };
    Ok(format!("took {permission} away from {id}{running}"))
}

/// Tells each run of app `id` of user `uid` that watches that `permission`
/// was taken away; how many were told.
fn tell_watchers(shared: &Shared, uid: u32, id: &str, permission: &str) -> usize {
    let line = Answer::Revoked(permission.into()).bytes();
    let mut told = 0;
    shared.watchers().retain(|w| {
        if w.uid != uid || w.id != id {
            return true;
        }
        let sent = (&w.stream).write_all(&line).is_ok();
        told += usize::from(sent);
        sent
    });
    told
}

/// Keeps a run's `watch` connection, after forgetting those whose runs
/// ended: one that has something to read has ended, as a runner sends
/// nothing on it. Its writes never wait: a runner that does not read is
/// forgotten rather than hold up every revoke.
fn watch(shared: &Shared, uid: u32, id: String, stream: UnixStream) {
    if stream.set_nonblocking(true).is_err() {
        return;
    }
    let mut watchers = shared.watchers();
    watchers.retain(|w| readable(&w.stream, Instant::now()).is_err());
    let theirs = watchers.iter().filter(|w| w.uid == uid).count();
    if theirs >= WATCHERS_MAX
        && let Some(oldest) = watchers.iter().position(|w| w.uid == uid)
    {
        watchers.remove(oldest);
    }
    watchers.push(Watcher { uid, id, stream });
}

/// Answers one connection.
fn serve(shared: &Shared, stream: UnixStream) {
    let Some(asker) = asker(&stream) else { return };
    let deadline = Instant::now() + REQUEST_TIME;
    let mut fds = Vec::new();
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 512];
    while !bytes.contains(&b'\n') && bytes.len() <= LINE_MAX {
        if readable(&stream, deadline).is_err() {
            return;
        }
        match signin::receive(&stream, &mut buffer, &mut fds) {
            Ok(0) | Err(_) => return,
            Ok(n) => bytes.extend_from_slice(&buffer[..n]),
        }
    }
    let Some(end) = bytes.iter().position(|&b| b == b'\n') else { return };
    let rest = bytes.split_off(end + 1);
    bytes.pop();
    let Ok(input) = stream.try_clone() else { return };
    let input = Timed { stream: input, deadline };
    let mut connection = Connection { stream, input: BufReader::new(input) };
    if !rest.is_empty() {
        // Nothing comes after a request but an answer to a question.
        let _ = connection.send(&Answer::Error("one request at a time".into()));
        return;
    }
    let request = String::from_utf8(bytes).map_err(|_| "not text".to_string());
    let request = request.and_then(|line| Request::read(&line));
    let result = match request {
        Err(why) => Err(why),
        Ok(Request::Install { everyone, yes }) => match fds.pop() {
            Some(fd) if fds.is_empty() => {
                install(shared, &mut connection, asker, &File::from(fd), everyone, yes)
            }
            _ => Err("an install request comes with its archive, and nothing else".into()),
        },
        Ok(_) if !fds.is_empty() => Err("only an install request comes with a file".into()),
        Ok(Request::List) => list(shared, &mut connection, asker),
        Ok(Request::Info { id }) => info(shared, &mut connection, asker, &id),
        Ok(Request::Remove { id, everyone, purge }) => remove(shared, asker, &id, everyone, purge),
        Ok(Request::Run { find: what }) => find(shared, &mut connection, asker, &what),
        Ok(Request::Ask { id, permission, list }) => {
            ask(shared, &connection, asker, &id, &permission, &list)
        }
        Ok(Request::Grant { id, permission, words }) => {
            grant(shared, &connection, asker, &id, &permission, words)
        }
        Ok(Request::Revoke { id, permission }) => revoke(shared, asker, &id, &permission),
        Ok(Request::Watch { id }) => {
            if installed_for(shared, asker, &id).is_ok()
                && connection.send(&Answer::Ok(format!("watching {id}"))).is_ok()
            {
                watch(shared, asker, id, connection.stream);
            }
            return;
        }
        Ok(Request::DeveloperKey { key }) => {
            let mut registry = shared.lock();
            registry.set_developer_key(asker, key);
            save(&registry).map(|()| format!("kept your developer key {}", key.encoded()))
        }
    };
    let answer = match result {
        Ok(what) => Answer::Ok(what),
        Err(why) => Answer::Error(why),
    };
    let _ = connection.send(&answer);
}

/// Forgets the bundles whose folders went while bundled was not running.
fn notice(registry: &mut Registry) -> bool {
    let gone: Vec<(Place, String)> = registry
        .installed
        .iter()
        .filter(|i| {
            place_folder(i.place).is_ok_and(|(path, _)| {
                fs::symlink_metadata(format!("{path}/{}", i.id))
                    .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
            })
        })
        .map(|i| (i.place, i.id.clone()))
        .collect();
    for (place, id) in &gone {
        if let Some(i) = registry.forget(*place, id) {
            audit!("forgot {id} {} of {}: its folder is gone", i.version, whom(*place));
        }
    }
    !gone.is_empty()
}

pub fn main(args: &mut Args) -> Result<i32, Fail> {
    if let Some(arg) = args.next()? {
        return Err(args.unknown(&arg));
    }
    // SAFETY: setting a signal's disposition touches no memory; no other
    // thread runs yet. A client that goes away must not end bundled.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(registry::STATE)
        .map_err(|e| util::failed(&registry::STATE, e))?;
    let texts = [registry::INSTALLED, registry::BOUND, registry::DEVELOPER_KEYS]
        .map(|name| read_state(name).map_err(|e| util::failed(&state(name), e)));
    let [installed, bound, keys] = texts;
    let mut registry = Registry::read(&installed?, &bound?, &keys?).map_err(util::fail)?;
    if notice(&mut registry) {
        save(&registry).map_err(util::fail)?;
    }
    let grants = read_grants()?;
    if let Ok(dir) = Dir::open(EVERYONE) {
        tidy(&dir);
    }
    // The links, from what it recorded: for everyone, and for each person
    // with a bundle.
    let mut places: Vec<Place> = registry.installed.iter().map(|i| i.place).collect();
    places.push(Place::Everyone);
    places.sort();
    places.dedup();
    for place in places {
        if let Err(why) = link_commands(&registry, place) {
            eprintln!("bundled: the commands of {}: {why}", whom(place));
        }
    }
    let socket = protocol::SOCKET;
    let folder = Path::new(socket).parent().unwrap_or(Path::new("/"));
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(folder)
        .map_err(|e| util::failed(&folder.display(), e))?;
    let _ = fs::remove_file(socket);
    let listener = UnixListener::bind(socket).map_err(|e| util::failed(&socket, e))?;
    // Anyone may ask: it tells who asks by the connection.
    fs::set_permissions(socket, fs::Permissions::from_mode(0o666))
        .map_err(|e| util::failed(&socket, e))?;
    eprintln!("bundled: {} bundles installed; answering at {socket}", registry.installed.len());
    let shared = Arc::new(Shared {
        registry: Mutex::new(registry),
        grants: Mutex::new(grants),
        watchers: Mutex::new(Vec::new()),
        connections: AtomicUsize::new(0),
    });
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if shared.connections.fetch_add(1, Ordering::SeqCst) >= CONNECTIONS_MAX {
            shared.connections.fetch_sub(1, Ordering::SeqCst);
            let mut stream = stream;
            let _ = stream.write_all(&Answer::Error("bundled is busy: try again".into()).bytes());
            continue;
        }
        let answering = Arc::clone(&shared);
        let started = std::thread::Builder::new().name("answer".into()).spawn(move || {
            serve(&answering, stream);
            answering.connections.fetch_sub(1, Ordering::SeqCst);
        });
        if started.is_err() {
            shared.connections.fetch_sub(1, Ordering::SeqCst);
            eprintln!("bundled: cannot start a thread for a connection");
        }
    }
    Ok(0)
}
