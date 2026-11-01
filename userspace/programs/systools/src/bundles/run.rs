// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Running a bundle (ADR 0225): `bundle run`, and a bundle's command by
//! its name, through the link bundled(8) made for it.
//!
//! bundled says which bundle it is, where, and the SHA-256 of the signed
//! list it recorded at install. The runner believes nothing else: it
//! opens the bundle's folder, not through a link, and checks that it is
//! still sealed (root's, mode 0555), that its list is the one recorded and
//! signed by the key recorded, and that its manifest is the one the list
//! names. The program it runs is a regular file of root's that nobody may
//! change, named in the list.
//!
//! A command that runs with your access is executed as a program of the
//! system's. Any other runs in the bundle's sandbox: a process of its own
//! whose root is a view (ADR 0215) of the system's commands, libraries,
//! resources and settings, read-only, the devices every program has, the
//! bundle's folder, read-only, its data folder, `~/app-data/ID`, which it
//! may change, and a `/tmp` of its own, each at its real path; with the
//! network only as its manifest asks, a clean environment, and the
//! terminal as descriptors 0 to 2. Until grants and the trusted prompts
//! come (13d.4), it gets only the permissions given at install.

use std::ffi::OsString;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;

use manifest::bundle::permissions::{self, Tier};
use manifest::bundle::{Arch, Manifest, Need, RunsWith};
use manifest::service::Ports;

use super::folder::{Dir, fd_stat};
use super::protocol::{Answer, Find, Found, Request};
use super::signed::{self, LIST_MAX, MANIFEST_MAX, SIGNATURE_MAX};
use super::sums::{self, Entry, LIST, SIGNATURE};
use super::{MANIFEST, answer, ask};
use crate::bundled;
use crate::util::{self, Fail, fail};

/// The architecture this system runs.
const ARCH: Arch = Arch::X86_64;

/// What a sandboxed program's view holds besides its bundle, its data
/// and its `/tmp`, each at its path, and whether it may change it: the
/// system's own, read-only, and the devices every program has. A place
/// this system lacks is left out.
#[cfg_attr(not(target_os = "radix"), allow(dead_code))]
pub const ALWAYS: &[(&str, bool)] = &[
    ("/system/commands", false),
    ("/system/libraries", false),
    ("/system/resources", false),
    ("/system/settings", false),
    ("/system/live/notify", false),
    ("/processes", false),
    ("/kernel", false),
    ("/devices/null", true),
    ("/devices/zero", false),
    ("/devices/random", false),
    ("/devices/urandom", false),
];

/// What a bundle that connects out also gets: resolved's folder, which
/// holds the resolver's settings that `/system/settings/resolv.conf`
/// leads to (ADR 0193), and netd's, which an older one leads to.
#[cfg_attr(not(target_os = "radix"), allow(dead_code))]
pub const NETWORK: &[&str] = &["/system/live/resolved", "/system/live/netd"];

/// `bundle run [--command NAME] ID [ARGUMENT...]`.
pub fn run(id: &str, command: Option<&str>, args: Vec<OsString>) -> Result<i32, Fail> {
    manifest::bundle::check_id(id).map_err(util::usage)?;
    let found = locate(&Find::Id(id.into()))?;
    start(&found, command, args)
}

/// A bundle's command, run as `name` through its link (`notes` for
/// `org.example.notes`'s): its exit status.
pub fn command(name: &str, args: Vec<OsString>) -> i32 {
    let result = manifest::bundle::check_command_name(name)
        .map_err(fail)
        .and_then(|()| locate(&Find::Command(name.into())))
        .and_then(|found| start(&found, Some(name), args));
    match result {
        Ok(code) => code,
        Err(failure) => {
            eprintln!("{name}: {}", failure.message);
            if failure.code == 0 { 1 } else { failure.code }
        }
    }
}

/// Asks bundled which bundle `find` names.
fn locate(find: &Find) -> Result<Found, Fail> {
    let (_stream, mut input) = ask(&Request::Run { find: find.clone() }, None).map_err(fail)?;
    let mut found = None;
    loop {
        match answer(&mut input)? {
            Answer::Found(f) if found.is_none() => found = Some(f),
            Answer::Ok(_) => break,
            Answer::Error(why) => return Err(fail(why)),
            _ => return Err(fail("bundled answered out of turn".into())),
        }
    }
    let found = found.ok_or_else(|| fail("bundled found nothing".into()))?;
    let asked = match find {
        Find::Id(id) => found.id == *id,
        Find::Command(_) => true,
    };
    if !asked {
        return Err(fail("bundled found another bundle than the one asked for".into()));
    }
    Ok(found)
}

/// A bundle's folder, checked: still as bundled sealed it, its list the
/// one recorded, its manifest the one the list names.
pub struct Sealed {
    pub folder: Dir,
    /// Its path.
    pub path: String,
    pub manifest: Manifest,
    pub listed: Vec<Entry>,
}

/// Whether `st` is root's and nobody may change it.
fn sealed_mode(st: &libc::stat) -> bool {
    st.st_uid == 0 && st.st_mode & 0o222 == 0
}

/// Reads the file `name` of the sealed folder, which must be sealed too.
fn sealed_file(folder: &Dir, path: &str, name: &str, max: u64) -> Result<Vec<u8>, String> {
    let (bytes, st) = folder.read(name, max).map_err(|e| format!("{path}/{name}: {e}"))?;
    if !sealed_mode(&st) {
        return Err(format!("{path}/{name} is not sealed: bundled did not install it so"));
    }
    Ok(bytes)
}

/// Opens and checks the bundle `found` names.
pub fn open_sealed(found: &Found) -> Result<Sealed, String> {
    let (places, owner) = bundled::place_folder(found.place)?;
    let dir = Dir::open(&places).map_err(|e| format!("{places}: {e}"))?;
    if dir.stat().map_err(|e| format!("{places}: {e}"))?.st_uid != owner {
        return Err(format!("{places} is not its owner's"));
    }
    let path = format!("{places}/{}", found.id);
    let folder = dir.dir_at(&found.id).map_err(|e| format!("{path}: {e}"))?;
    let st = folder.stat().map_err(|e| format!("{path}: {e}"))?;
    if st.st_uid != 0 || st.st_mode & 0o7777 != 0o555 {
        return Err(format!("{path} is not sealed: bundled did not install it so"));
    }
    let list = sealed_file(&folder, &path, LIST, LIST_MAX)?;
    if sums::hex(&sums::sha256(&list)) != found.list {
        return Err(format!("{path}: its signed list is not the one bundled installed"));
    }
    let signature = sealed_file(&folder, &path, SIGNATURE, SIGNATURE_MAX)?;
    let (key, _, listed) = signed::check_list(&list, &signature)?;
    if key != found.key {
        return Err(format!("{path}: it is signed by another key than the one installed"));
    }
    let bytes = sealed_file(&folder, &path, MANIFEST, MANIFEST_MAX)?;
    let manifest = signed::manifest_of(&bytes, &listed)?;
    if manifest.id != found.id {
        return Err(format!("{path}: its manifest is {}'s", manifest.id));
    }
    Ok(Sealed { folder, path, manifest, listed })
}

/// The program to run, and how.
#[derive(Debug, PartialEq, Eq)]
pub struct Chosen {
    /// Its name among the bundle's programs.
    pub program: String,
    /// Its path in the bundle.
    pub path: String,
    pub runs_with: RunsWith,
    /// The name it is run by: its command's, or its own.
    pub arg0: String,
}

/// The program `command` runs: that command's; or with none, the
/// bundle's first command's, or its only program.
pub fn choose(manifest: &Manifest, command: Option<&str>) -> Result<Chosen, String> {
    let id = &manifest.id;
    let (arg0, program, runs_with) = match command {
        Some(name) => {
            let found = manifest.commands.iter().find(|c| c.name == *name);
            let c = found.ok_or_else(|| format!("{id} adds no command {name}"))?;
            (c.name.to_string(), c.program.to_string(), c.runs_with)
        }
        None => match manifest.commands.first() {
            Some(c) => (c.name.to_string(), c.program.to_string(), c.runs_with),
            None if manifest.programs.len() == 1 => {
                let (name, _) = manifest.programs.iter().next().expect("one program");
                (name.to_string(), name.to_string(), RunsWith::Sandbox)
            }
            None => return Err(format!("{id} adds no command, so there is none to run")),
        },
    };
    let found = manifest.programs.get(program.as_str());
    let p = found.ok_or_else(|| format!("{id} has no program {program}"))?;
    let path = p
        .path
        .get(&ARCH)
        .ok_or_else(|| format!("{id} has no {program} for {} computers", ARCH.word()))?;
    Ok(Chosen { program, path: path.0.clone(), runs_with, arg0 })
}

/// What a sandboxed program is given of the permissions its bundle asks
/// for.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Given {
    /// `network.outbound`: to connect out, on every port.
    pub outbound: bool,
    /// `network.listen`: the ports, each with whether it is required.
    pub listen: Vec<(Ports, bool)>,
    /// The optional permissions it runs without, by name.
    pub without: Vec<String>,
}

/// What `program` of `manifest` is given: the permissions given at
/// install, and those always given, as many as the program keeps
/// (`programs.NAME.permissions`); one asked of another tier, which only a
/// grant gives (13d.4), stops it if it is required.
pub fn given(manifest: &Manifest, program: &str) -> Result<Given, String> {
    let narrowed = manifest.programs.get(program).map_or(&[][..], |p| p.permissions.as_slice());
    let mut given = Given::default();
    for (name, request) in manifest.permissions.iter() {
        if !narrowed.is_empty() && !narrowed.iter().any(|n| n == name) {
            continue;
        }
        let Some(permission) = permissions::find(name) else { continue };
        let required = request.need == Need::Required;
        match permission.tier {
            Tier::AtInstall | Tier::Always => match name {
                "network.outbound" => given.outbound = true,
                "network.listen" => {
                    given.listen.extend(request.ports.iter().map(|&p| (p, required)));
                }
                // Notifications are always allowed; starting at sign-in
                // is the session's to do, not the program's.
                _ => {}
            },
            _ if required => {
                return Err(format!(
                    "{} needs \"{}\" ({name}), which this version of Dukovo cannot grant yet",
                    manifest.id, permission.lets
                ));
            }
            _ => given.without.push(name.into()),
        }
    }
    Ok(given)
}

/// The environment a sandboxed program starts with: a clean one (ADR
/// 0225), its data folder as its home, and the caller's terminal type,
/// language and time zone.
pub fn environment(user: &accounts::User, home: &str, caller: &[(String, String)]) -> Vec<String> {
    let mut env = vec![
        format!("HOME={home}"),
        format!("USER={}", user.name),
        format!("LOGNAME={}", user.name),
        "PATH=/system/commands".into(),
    ];
    let term = caller.iter().find(|(name, _)| name == "TERM").map_or("", |(_, v)| v.as_str());
    env.push(format!("TERM={}", crate::signin::terminal_type(term)));
    for (name, value) in caller {
        let entry = format!("{name}={value}");
        let zone = name == "TZ"
            && value.len() <= 64
            && value.chars().all(|c| c.is_ascii_alphanumeric() || "/_+-:,.".contains(c));
        if zone || crate::elevate::language_setting(&entry) {
            env.push(entry);
        }
    }
    env
}

/// Opens the program at `path` in the sealed folder, one part at a time,
/// never through a link: a regular file of root's, nobody's to change,
/// that may run, and that the signed list names.
fn open_program(sealed: &Sealed, path: &str) -> Result<OwnedFd, String> {
    let shown = format!("{}/{path}", sealed.path);
    if !sealed.listed.iter().any(|e| e.path == path) {
        return Err(format!("{shown}: its signed list does not name it"));
    }
    let mut parts: Vec<&str> = path.split('/').collect();
    let name = parts.pop().ok_or_else(|| format!("{shown}: no file"))?;
    let mut dirs: Vec<Dir> = Vec::new();
    for part in parts {
        let at = dirs.last().unwrap_or(&sealed.folder);
        let next = at.dir_at(part).map_err(|e| format!("{shown}: {e}"))?;
        dirs.push(next);
    }
    let at = dirs.last().unwrap_or(&sealed.folder);
    let fd = at.open_at(name, false).map_err(|e| format!("{shown}: {e}"))?;
    let st = fd_stat(&fd).map_err(|e| format!("{shown}: {e}"))?;
    let regular = st.st_mode & libc::S_IFMT == libc::S_IFREG;
    if !regular || !sealed_mode(&st) || st.st_mode & 0o111 == 0 {
        return Err(format!("{shown} is not a sealed program"));
    }
    Ok(fd)
}

/// The caller's environment, as pairs.
fn caller_environment() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect()
}

/// Makes the folder `path` if it is not there, mode 0700, and checks that
/// it is a folder of `uid`'s, not a link.
fn own_folder(path: &str, uid: u32) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => {
            return Err(format!("{path}: {}", util::strip_os_error(&e)));
        }
        _ => {}
    }
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{path}: {e}"))?;
    if !meta.is_dir() || meta.uid() != uid {
        return Err(format!("{path} is not a folder of yours"));
    }
    Ok(())
}

/// Runs what `command` names of the bundle `found` names, with `args`.
fn start(found: &Found, command: Option<&str>, args: Vec<OsString>) -> Result<i32, Fail> {
    let sealed = open_sealed(found).map_err(fail)?;
    let chosen = choose(&sealed.manifest, command).map_err(fail)?;
    let program = open_program(&sealed, &chosen.path).map_err(fail)?;
    let path = format!("{}/{}", sealed.path, chosen.path);
    if chosen.runs_with == RunsWith::YourAccess {
        // As a program of the system's: with all the caller holds.
        use std::os::unix::process::CommandExt;
        drop(program);
        let error = std::process::Command::new(&path).arg0(&chosen.arg0).args(args).exec();
        return Err(util::failed(&path, error));
    }
    let given = given(&sealed.manifest, &chosen.program).map_err(fail)?;
    // SAFETY: getuid has no failure and touches no memory.
    let uid = unsafe { libc::getuid() };
    let user = bundled::account(uid).map_err(fail)?;
    let home = user.home.trim_end_matches('/');
    let app_data = format!("{home}/{}", bundled::APP_DATA);
    own_folder(&app_data, uid).map_err(fail)?;
    let data = format!("{app_data}/{}", found.id);
    own_folder(&data, uid).map_err(fail)?;
    let tmp = format!("/tmp/.app-{}", found.id);
    own_folder(&tmp, uid).map_err(fail)?;
    let env = environment(&user, &data, &caller_environment());
    let mut argv: Vec<Vec<u8>> = vec![chosen.arg0.clone().into_bytes()];
    argv.extend(args.iter().map(|a| a.as_bytes().to_vec()));
    let plan = Plan {
        sealed: &sealed,
        program: &program,
        path: &path,
        argv: &argv,
        env: &env,
        given: &given,
        data: &data,
        tmp: &tmp,
    };
    sandbox::run(&plan).map_err(|why| fail(format!("{}: {why}", found.id)))
}

/// What the sandbox is built from.
#[cfg_attr(not(target_os = "radix"), allow(dead_code))]
pub struct Plan<'a> {
    pub sealed: &'a Sealed,
    /// The program, open.
    pub program: &'a OwnedFd,
    /// Its path, the same in the view.
    pub path: &'a str,
    pub argv: &'a [Vec<u8>],
    pub env: &'a [String],
    pub given: &'a Given,
    /// Its data folder and its `/tmp`, which it may change.
    pub data: &'a str,
    pub tmp: &'a str,
}

#[cfg(not(target_os = "radix"))]
mod sandbox {
    use super::Plan;

    /// Built for another system, for tests: there is no sandbox to run in.
    pub fn run(_plan: &Plan<'_>) -> Result<i32, String> {
        Err("bundles run on Dukovo".into())
    }
}

#[cfg(target_os = "radix")]
mod sandbox {
    use std::os::fd::AsRawFd;

    use radix_abi::capability::{self, ObjectKind, Rights};
    use radix_abi::directory::{self, OpenFlags};
    use radix_abi::{Status, network, process, startup};

    use super::{ALWAYS, NETWORK, Plan};
    use crate::signin::{self, Spares};
    use crate::sys::{self, SYS};

    /// To read and change, and to copy and pass on, as `fork` and `exec`
    /// need; never the bypass right, and never control.
    const GIVEN: Rights = Rights(
        Rights::READ.bits() | Rights::WRITE.bits() | Rights::COPY.bits() | Rights::TRANSFER.bits(),
    );
    /// To read, and to copy and pass on.
    const READ_ONLY: Rights =
        Rights(Rights::READ.bits() | Rights::COPY.bits() | Rights::TRANSFER.bits());

    fn failed(what: String) -> impl Fn(Status) -> String {
        move |status| format!("{what}: {}", sys::status_name(status.0))
    }

    /// Slots for the work.
    struct Slots {
        base: u32,
        view: u32,
        given: u32,
        scratch: u32,
        child: u32,
        network: u32,
    }

    /// Builds the view, starts the program in it, and waits for it.
    pub fn run(plan: &Plan<'_>) -> Result<i32, String> {
        // Read before the spare slots are taken: an empty spare is a free
        // descriptor to the C library, which reading a folder would take.
        let links = links();
        let spares = Spares::new(6).map_err(|e| format!("no slots: {e}"))?;
        let [base, view, given, scratch, child, network] =
            [0, 1, 2, 3, 4, 5].map(|i| spares.slot(i));
        let slots = Slots { base, view, given, scratch, child, network };
        let number = build_and_start(plan, &slots, &links)?;
        drop(spares);
        Ok(wait(number))
    }

    /// The links of the root and of `/system`, by their paths without the
    /// leading slash, so `/bin/sh` and `/usr/bin/env` lead where they do.
    fn links() -> Vec<(String, Vec<u8>)> {
        let mut links = Vec::new();
        for dir in ["/", "/system"] {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(target) = std::fs::read_link(&path) else { continue };
                let Some(at) = path.to_str().map(|p| p.trim_start_matches('/').to_string()) else {
                    continue;
                };
                links.push((at, target.into_os_string().into_encoded_bytes()));
            }
        }
        links
    }

    fn build_and_start(
        plan: &Plan<'_>,
        s: &Slots,
        links: &[(String, Vec<u8>)],
    ) -> Result<u64, String> {
        // Without the bypass right or control, whatever this program's
        // root has: the view acts for whoever runs it, as the owners and
        // modes of its files let them.
        let rights =
            capability::info(&SYS, startup::SLOT_ROOT).map_err(failed("the root".into()))?.rights;
        let narrowed = Rights(rights.bits() & GIVEN.bits());
        capability::derive(&SYS, startup::SLOT_ROOT, s.base, narrowed, 0)
            .map_err(failed("narrowing the root".into()))?;
        directory::make_view(&SYS, s.base, s.view).map_err(failed("making its view".into()))?;
        // Opens `path` of the root and attaches it at the same path.
        let attach = |path: &str, write: bool| -> Result<(), String> {
            let at = path.trim_start_matches('/');
            let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{path}: {e}"))?;
            let flags = if meta.is_dir() { OpenFlags::DIRECTORY } else { OpenFlags::NONE };
            let rights = if write { GIVEN } else { READ_ONLY };
            let flags = OpenFlags(flags.bits() | OpenFlags::NO_FOLLOW.bits());
            directory::open(&SYS, s.base, at.as_bytes(), flags, rights, s.scratch, 0)
                .map_err(failed(format!("opening {path}")))?;
            let attached = directory::attach(&SYS, s.view, at.as_bytes(), s.scratch);
            let _ = capability::delete(&SYS, s.scratch);
            attached.map_err(failed(format!("attaching {path}")))
        };
        for &(path, write) in ALWAYS {
            if std::fs::symlink_metadata(path).is_ok() {
                attach(path, write)?;
            }
        }
        for path in NETWORK.iter().filter(|_| plan.given.outbound) {
            if std::fs::symlink_metadata(path).is_ok() {
                attach(path, false)?;
            }
        }
        // The bundle's folder: the very one checked, read-only.
        let folder = sys::slot_of(plan.sealed.folder.0.as_raw_fd());
        capability::derive(&SYS, folder, s.scratch, READ_ONLY, 0)
            .map_err(failed(plan.sealed.path.clone()))?;
        let at = plan.sealed.path.trim_start_matches('/');
        let attached = directory::attach(&SYS, s.view, at.as_bytes(), s.scratch);
        let _ = capability::delete(&SYS, s.scratch);
        attached.map_err(failed(format!("attaching {}", plan.sealed.path)))?;
        attach(plan.data, true)?;
        // Its own /tmp, at /tmp.
        let tmp = plan.tmp.trim_start_matches('/');
        let flags = OpenFlags(OpenFlags::DIRECTORY.bits() | OpenFlags::NO_FOLLOW.bits());
        directory::open(&SYS, s.base, tmp.as_bytes(), flags, GIVEN, s.scratch, 0)
            .map_err(failed(format!("opening {}", plan.tmp)))?;
        let attached = directory::attach(&SYS, s.view, b"tmp", s.scratch);
        let _ = capability::delete(&SYS, s.scratch);
        attached.map_err(failed("attaching /tmp".into()))?;
        for (at, target) in links {
            // A name the view has already keeps what it has.
            let _ = directory::attach_link(&SYS, s.view, at.as_bytes(), target);
        }
        // Without control: nothing more is attached to it.
        capability::derive(&SYS, s.view, s.given, GIVEN, 0)
            .map_err(failed("narrowing its view".into()))?;

        process::create(&SYS, startup::SLOT_PROCESS, ObjectKind::Process, signin::SLOTS, s.child)
            .map_err(failed("making its process".into()))?;
        let mut shares = vec![
            (s.given, startup::SLOT_ROOT),
            (startup::SLOT_RANDOM, startup::SLOT_RANDOM),
            (startup::SLOT_LOG, startup::SLOT_LOG),
        ];
        for fd in 0..3 {
            let slot = sys::slot_of(fd);
            if capability::info(&SYS, slot).is_ok() {
                shares.push((slot, startup::SLOT_FD_BASE + fd as u32));
            }
        }
        for (from, to) in shares {
            process::share(&SYS, s.child, from, to, Rights::ALL, 0)
                .map_err(failed("giving it its capabilities".into()))?;
        }
        give_network(plan, s)?;
        let argv: Vec<&[u8]> = plan.argv.iter().map(Vec::as_slice).collect();
        let block =
            signin::exec_block_in(plan.data.as_bytes(), plan.path.as_bytes(), &argv, plan.env);
        let program = sys::slot_of(plan.program.as_raw_fd());
        process::exec(&SYS, s.child, program, &block).map_err(failed("starting it".into()))?;
        let number = process::info(&SYS, s.child).map_err(failed("its number".into()))?.number;
        Ok(number)
    }

    /// The caller's network capabilities, in their slots: each with its
    /// rights and the ports it covers, from `first` to before `past`.
    fn held() -> Vec<(u32, Rights, u32, u32)> {
        let end = startup::SLOT_NETWORK + startup::NETWORK_SLOTS;
        (startup::SLOT_NETWORK..end)
            .filter_map(|slot| {
                let info = capability::info(&SYS, slot).ok()?;
                if info.kind != ObjectKind::Network {
                    return None;
                }
                let ports = network::ports(&SYS, slot).ok()?;
                Some((slot, info.rights, ports.first, ports.first.saturating_add(ports.count)))
            })
            .collect()
    }

    /// Gives the program the network its manifest asks for, out of what
    /// the caller holds, in its slots from SLOT_NETWORK: to connect out,
    /// then each range of ports to listen on, in order. A required range
    /// the caller cannot listen on stops it; an optional one is left out.
    fn give_network(plan: &Plan<'_>, s: &Slots) -> Result<(), String> {
        let kept = Rights::COPY.union(Rights::TRANSFER);
        let held = held();
        let end = startup::SLOT_NETWORK + startup::NETWORK_SLOTS;
        let mut to = startup::SLOT_NETWORK;
        let mut give = |from: u32, first: u32, count: u32, right: Rights| -> Result<(), String> {
            if to >= end {
                return Err("it asks for more ranges of ports than a program holds".into());
            }
            network::restrict(&SYS, from, s.network, first, count, right.union(kept))
                .map_err(failed("its network".into()))?;
            let shared = process::share(&SYS, s.child, s.network, to, Rights::ALL, 0);
            let _ = capability::delete(&SYS, s.network);
            shared.map_err(failed("giving it its network".into()))?;
            to += 1;
            Ok(())
        };
        if plan.given.outbound {
            for &(slot, rights, first, past) in &held {
                if rights.bits() & Rights::SEND.bits() != 0 && first < past {
                    give(slot, first, past - first, Rights::SEND)?;
                }
            }
        }
        for &(ports, required) in &plan.given.listen {
            let (first, past) = (u32::from(ports.first), u32::from(ports.last) + 1);
            let covering = held.iter().find(|&&(_, rights, from, until)| {
                rights.bits() & Rights::RECEIVE.bits() != 0 && from <= first && past <= until
            });
            match covering {
                Some(&(slot, ..)) => give(slot, first, past - first, Rights::RECEIVE)?,
                None if required => {
                    return Err(format!(
                        "it listens on port {ports}, which you may not listen on: below 1024, \
                         only an administrator's commands do"
                    ));
                }
                None => {}
            }
        }
        Ok(())
    }

    /// Waits for the program, and gives its exit status as a shell does:
    /// 128 and the signal's number for one a signal ended.
    fn wait(number: u64) -> i32 {
        // As system(3): the terminal's Ctrl+C is the program's to take.
        // SAFETY: setting dispositions touches no memory.
        let old = unsafe {
            (libc::signal(libc::SIGINT, libc::SIG_IGN), libc::signal(libc::SIGQUIT, libc::SIG_IGN))
        };
        let mut status = 0;
        let pid = i32::try_from(number).unwrap_or(-1);
        loop {
            // SAFETY: waitpid writes the status.
            let r = unsafe { libc::waitpid(pid, &raw mut status, 0) };
            if r >= 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                break;
            }
        }
        // SAFETY: as above.
        unsafe {
            libc::signal(libc::SIGINT, old.0);
            libc::signal(libc::SIGQUIT, old.1);
        }
        if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else if libc::WIFSIGNALED(status) {
            128 + libc::WTERMSIG(status)
        } else {
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(extra: &str, permissions: &str) -> Manifest {
        let text = manifest::bundle::template("org.example.notes", "Notes", "Example", "MIT")
            .replace("\"permissions\": {}", &format!("\"permissions\": {permissions}"))
            .replace("\"data\":", &format!("{extra}\"data\":"));
        manifest::bundle::from_json(&text).unwrap_or_else(|e| panic!("{e:?}\n{text}"))
    }

    #[test]
    fn the_command_chooses_the_program() {
        let m = manifest("", "{}");
        let chosen = choose(&m, None).unwrap();
        assert_eq!(
            chosen,
            Chosen {
                program: "notes".into(),
                path: "bin/notes".into(),
                runs_with: RunsWith::Sandbox,
                arg0: "notes".into(),
            }
        );
        assert_eq!(choose(&m, Some("notes")).unwrap(), chosen);
        assert_eq!(choose(&m, Some("ls")).unwrap_err(), "org.example.notes adds no command ls");
    }

    #[test]
    fn only_what_install_gives_is_given() {
        let m = manifest("", "{}");
        assert_eq!(given(&m, "notes").unwrap(), Given::default());
        let net = r#"{"network.outbound": {"need": "required"},
            "network.listen": {"need": "optional", "ports": [8080, "9000-9009"]},
            "notifications": {"need": "optional"}}"#;
        let g = given(&manifest("", net), "notes").unwrap();
        assert!(g.outbound);
        let ports: Vec<String> = g.listen.iter().map(|(p, _)| p.to_string()).collect();
        assert_eq!(ports, ["8080", "9000-9009"]);
        assert!(g.listen.iter().all(|&(_, required)| !required));
        // Asked on first use: none until grants come; required, it stops.
        let folder = r#"{"files.folder": {"need": "optional", "folders": {"documents": "read"}}}"#;
        let g = given(&manifest("", folder), "notes").unwrap();
        assert_eq!(g.without, ["files.folder"]);
        let required = folder.replace("optional", "required");
        let why = given(&manifest("", &required), "notes").unwrap_err();
        assert!(why.contains("cannot grant yet"), "{why}");
    }

    #[test]
    fn a_program_narrows_itself() {
        let text = manifest::bundle::template("org.example.notes", "Notes", "Example", "MIT")
            .replace(
                "\"path\": { \"x86_64\": \"bin/notes\" } }",
                "\"path\": { \"x86_64\": \"bin/notes\" }, \"permissions\": [\"network.listen\"] }",
            )
            .replace(
                "\"permissions\": {}",
                r#""permissions": {"network.outbound": {"need": "required"},
                    "network.listen": {"need": "required", "ports": [8080]}}"#,
            );
        let m = manifest::bundle::from_json(&text).unwrap_or_else(|e| panic!("{e:?}\n{text}"));
        let g = given(&m, "notes").unwrap();
        assert!(!g.outbound);
        assert_eq!(g.listen.len(), 1);
    }

    #[test]
    fn the_environment_is_clean() {
        let user = accounts::User {
            name: "alice".into(),
            uid: 1000,
            gid: 1000,
            full_name: String::new(),
            home: "/users/alice".into(),
            shell: "/system/commands/nukesh".into(),
        };
        let caller: Vec<(String, String)> = [
            ("TERM", "linux"),
            ("LANG", "en_US.UTF-8"),
            ("LC_TIME", "C"),
            ("TZ", "Europe/Paris"),
            ("SECRET", "x"),
            ("LD_PRELOAD", "/tmp/x.so"),
            ("PATH", "/users/alice/bin"),
        ]
        .iter()
        .map(|(k, v)| ((*k).into(), (*v).into()))
        .collect();
        let env = environment(&user, "/users/alice/app-data/org.example.notes", &caller);
        assert_eq!(
            env,
            [
                "HOME=/users/alice/app-data/org.example.notes",
                "USER=alice",
                "LOGNAME=alice",
                "PATH=/system/commands",
                "TERM=linux",
                "LANG=en_US.UTF-8",
                "LC_TIME=C",
                "TZ=Europe/Paris",
            ]
        );
    }
}
