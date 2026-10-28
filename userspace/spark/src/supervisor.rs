// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Spark as the supervisor of the system's services (Phase 9, ADR 0101).
//!
//! Each service runs in a resource domain of its own below `services`,
//! the domain the kernel gave Spark (radix-resource-domain(2)), with the
//! CPU weight, memory limit and out-of-memory class its file gives
//! (`services`). Its standard output and error are a pipe whose lines
//! Spark copies to the kernel log, its input is _/devices/null_. When it ends
//! Spark starts it again as its file says, waiting 0.1 s after a quick
//! end, then twice as long after each one in a row, up to 30 s; a service
//! the out-of-memory policy stopped ends as if killed, and is started
//! again the same way.
//!
//! `service stop` and `restart` send SIGTERM, and stop the service's
//! domain if it has not ended after STOP_WAIT, as systemd sends SIGKILL
//! after TimeoutStopSec. Once a service's process ends, the processes it
//! left in its domain end too, as systemd's default KillMode=control-group
//! ends them: Spark stops the domain, and starts the service again only
//! once the domain is empty.
//!
//! Spark hears of its children's ends from a notification the kernel
//! signals (radix-process(2)'s `notify_children`), and waits in `poll`
//! for it, the services' output, the program it was asked to run, and the
//! control socket, `/system/live/spark/control`: shutdown(8), reboot(8),
//! poweroff(8), halt(8) and service(8) send a request there, one line, and
//! read the answer. Shutting down stops the services the highest `order`
//! first, each group with SIGTERM and then, after STOP_WAIT, by stopping
//! its domain, then calls the power capability (radix-power(2)). Pressing
//! the power button, which the kernel tells of through another
//! notification (`notify_button`), powers the machine off the same way,
//! at once, as systemd-logind's default HandlePowerKey=poweroff does.
//!
//! Once it supervises, Spark starts the hardware watchdog, if the kernel
//! drives one, with a timeout of WATCHDOG_SECONDS, and pets it each third
//! of that from its loop (ADR 0143): a Spark that stops responding, or a
//! kernel that stops running it, restarts the machine, as systemd's
//! RuntimeWatchdogSec does.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use libradix::abi::capability::{self, ObjectKind, Rights};
use libradix::abi::directory::{FileKind, OpenFlags};
use libradix::abi::file::{self as file_abi, DescriptionFlags};
use libradix::abi::notification::WaitFlags;
use libradix::abi::process::{
    self, ChildWaitFlags, ClockKind, PipeFlags, PollEntry, PollEvents, PollFlags,
};
use libradix::abi::resource_domain::{self, NO_LIMIT};
use libradix::abi::startup::{
    SLOT_FD_BASE, SLOT_FIRST_EXTRA, SLOT_LOG, SLOT_PROCESS, SLOT_RANDOM, SLOT_ROOT,
};
use libradix::abi::{power, signal, syscalls};
use radix_rt::fs::{Dir, File};
use radix_rt::{SYS, println, slots};

use crate::services::{self, Restart, Spec};

/// Where the service files are.
pub const SERVICES_DIR: &str = "system/settings/spark/services";

/// The control socket, and the directory it is in.
const CONTROL_DIR: &str = "system/live/spark";
const CONTROL: &str = "system/live/spark/control";

/// The slots a POSIX program's capability space starts with: room for 64
/// file descriptors, as Linux's first table of them has. The C library
/// grows the space as more are opened, so a program pays for the room it
/// uses, and a fork copies no more.
pub const POSIX_SLOTS: u32 = 128;

/// The environment a POSIX program starts with: text is UTF-8 (the
/// C.UTF-8 locale, radix-posix(7)), and nano is the editor programs
/// open a file in for its user (ADR 0090).
pub const POSIX_ENV: [&str; 5] =
    ["PATH=/system/commands", "HOME=/", "TERM=dumb", "LANG=C.UTF-8", "EDITOR=nano"];

const MS: u64 = 1_000_000;
const SECOND: u64 = 1_000 * MS;

/// The hardware watchdog's timeout, unless `--watchdog` gives another.
pub const WATCHDOG_SECONDS: u32 = 60;

/// A service that ran this long before it ended starts again at once.
const STEADY: u64 = 10 * SECOND;
/// The first wait before a service that ended quickly starts again, and
/// the longest.
const BACKOFF_FIRST: u64 = 100 * MS;
const BACKOFF_MAX: u64 = 30 * SECOND;
/// How long stopping a service, or a group of them when shutting down,
/// waits for it to end after SIGTERM, before it stops its domain.
const STOP_WAIT: u64 = 5 * SECOND;
/// How often Spark looks whether what a service left behind has ended.
const DRAIN_POLL: u64 = 100 * MS;
/// How long it waits after stopping them.
const KILL_WAIT: u64 = SECOND;

/// The longest line of a service's output kept; longer ones are cut.
const LINE_MAX: usize = 1024;

/// The most control connections served at once.
const CONNECTIONS_MAX: usize = 8;
/// How long a control connection may take to send its request: one that
/// sends nothing does not keep its place for good.
const CONNECTION_WAIT: u64 = 10 * SECOND;

/// The monotonic clock, in nanoseconds.
pub fn now() -> u64 {
    process::clock(&SYS, SLOT_PROCESS, ClockKind::Monotonic).map_or(0, |c| c.ns)
}

/// What a wait status says, in words.
fn ending(status: u32) -> String {
    if status & 0x7f == 0 {
        format!("exited with code {:#x}", status >> 8 & 0xff)
    } else {
        format!("was ended by signal {}", status & 0x7f)
    }
}

/// Whether a wait status is a failure: an exit code other than 0, or a
/// signal.
fn failed(status: u32) -> bool {
    status != 0
}

/// The block `exec` takes (radix-process(2)): `path` and `args` as the
/// arguments, POSIX_ENV with `env` over it, and the working directory `/`
/// and `path` as the extra strings.
pub fn exec_block(path: &str, args: &[&str], env: &[String]) -> Vec<u8> {
    let mut environment: Vec<&str> = POSIX_ENV
        .iter()
        .copied()
        .filter(|base| {
            let name = base.split('=').next().unwrap_or("");
            !env.iter().any(|e| e.split('=').next() == Some(name))
        })
        .collect();
    environment.extend(env.iter().map(String::as_str));
    let mut block = Vec::new();
    let header = [process::EXEC_VERSION, 1 + args.len() as u32, environment.len() as u32, 2];
    for word in header {
        block.extend_from_slice(&word.to_le_bytes());
    }
    let strings =
        [path].into_iter().chain(args.iter().copied()).chain(environment).chain(["/", path]);
    for string in strings {
        block.extend_from_slice(string.as_bytes());
        block.push(0);
    }
    block
}

/// Makes the description in `slot` not wait.
fn non_blocking(slot: u32) {
    let _ = file_abi::status_flags(&SYS, slot, true, DescriptionFlags::NON_BLOCKING);
}

/// Where a service is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Not running, and not to be started: turned off, stopped by a
    /// request, ended for good, or never started.
    Stopped,
    /// Running as this process.
    Running(u64),
    /// To start at this time.
    Waiting(u64),
    /// Its process ended with this status, and Spark stopped its domain,
    /// where processes it left still run; once they have gone, it is
    /// started again or not, as the status says. Spark looks again at the
    /// time given.
    Draining(u32, u64),
}

/// A service and its process.
struct Service {
    spec: Spec,
    state: State,
    /// Its domain's capability, once it is made; it stays for restarts.
    domain: Option<u32>,
    /// The read end of its output pipe.
    output: Option<File>,
    line: Vec<u8>,
    started_ns: u64,
    /// Quick ends in a row, which the backoff doubles with.
    quick: u32,
    restarts: u32,
    last: Option<u32>,
    /// Stopped by a request: not started again until asked.
    held: bool,
    /// Asked to restart: started again whatever its file says.
    restarting: bool,
    /// Asked to stop or restart: when Spark stops its domain if it has
    /// not ended.
    stop_by: Option<u64>,
    /// Spark stopped its domain, so what is left in it is ending already.
    killed: bool,
    /// How many times the out-of-memory policy had stopped its domain.
    oom_stops: u32,
}

/// The program Spark was asked to run, on a pseudo-terminal, until it
/// ends.
pub struct Program {
    pub path: String,
    pub number: u64,
    pub master: File,
    pub line: Vec<u8>,
    /// Every descriptor of its terminal has closed: the master is not
    /// polled any more, which would say so again at once, for good.
    pub hung_up: bool,
}

/// What shutting down ends with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    PowerOff,
    Restart,
    Halt,
}

impl End {
    fn words(self) -> &'static str {
        match self {
            End::PowerOff => "power off",
            End::Restart => "restart",
            End::Halt => "halt",
        }
    }
}

/// A shutdown asked for.
#[derive(Clone, Copy, Debug)]
enum Shutdown {
    /// To start at this time.
    Pending { end: End, at: u64 },
    /// Stopping the services of `order` and above; `deadline` is when it
    /// stops their domains (`killed` once it has).
    Stopping { end: End, order: u32, deadline: u64, killed: bool },
}

/// A connection to the control socket, what it has sent so far, and
/// when it is closed if its request is not whole by then.
struct Connection {
    file: File,
    request: Vec<u8>,
    until: u64,
}

/// Spark's supervision.
pub struct Supervisor {
    root: Dir,
    /// The `services` domain and the power capability, from the kernel.
    services_domain: Option<u32>,
    power: Option<u32>,
    /// The notification the kernel signals when a child changes.
    children: Option<u32>,
    /// The notification the kernel signals when the power button is
    /// pressed, if it can hear it.
    button: Option<u32>,
    listener: Option<File>,
    connections: Vec<Connection>,
    services: Vec<Service>,
    program: Option<Program>,
    shutdown: Option<Shutdown>,
    /// The watchdog's timeout to ask for, 0 for none, and whether to stop
    /// responding once it runs (`--test-hang`).
    watchdog_seconds: u32,
    hang: bool,
    /// While the watchdog runs: how often Spark pets it, and when next.
    pets: Option<(u64, u64)>,
}

impl Supervisor {
    /// Spark's supervision of the tree under `root`, with the capabilities
    /// the kernel gave it from SLOT_FIRST_EXTRA on.
    pub fn new(root: Dir) -> Supervisor {
        let holds = |slot: u32, kind: ObjectKind| {
            capability::info(&SYS, slot).is_ok_and(|info| info.kind == kind)
        };
        let domain = SLOT_FIRST_EXTRA + 1;
        let power = SLOT_FIRST_EXTRA + 2;
        let children = slots::take().filter(|&slot| {
            process::create(&SYS, SLOT_PROCESS, ObjectKind::Notification, 0, slot).is_ok()
                && process::notify_children(&SYS, SLOT_PROCESS, slot).is_ok()
        });
        if children.is_none() {
            println!("spark: cannot hear of children ending; services are not watched");
        }
        let power = holds(power, ObjectKind::Power).then_some(power);
        // Not supported where the kernel cannot hear the button: the
        // firmware keeps it.
        let button = power.and_then(|power| {
            slots::take().filter(|&slot| {
                process::create(&SYS, SLOT_PROCESS, ObjectKind::Notification, 0, slot).is_ok()
                    && power::notify_button(&SYS, power, slot).is_ok()
            })
        });
        Supervisor {
            root,
            services_domain: holds(domain, ObjectKind::ResourceDomain).then_some(domain),
            power,
            children,
            button,
            listener: None,
            connections: Vec::new(),
            services: Vec::new(),
            program: None,
            shutdown: None,
            watchdog_seconds: WATCHDOG_SECONDS,
            hang: false,
            pets: None,
        }
    }

    /// Sets the watchdog's timeout, 0 to leave it off, and whether Spark
    /// stops responding once it runs.
    pub fn set_watchdog(&mut self, seconds: u32, hang: bool) {
        self.watchdog_seconds = seconds;
        self.hang = hang;
    }

    /// Starts the watchdog, if the kernel drives one. With `--test-hang`,
    /// Spark then stops responding, unless the watchdog restarted the
    /// machine at the end of the last boot.
    fn start_watchdog(&mut self) {
        let Some(power) = self.power else { return };
        if self.watchdog_seconds == 0 {
            return;
        }
        // Not supported without a watchdog: the kernel logs that it has
        // none.
        let Ok(started) = power::start_watchdog(&SYS, power, self.watchdog_seconds) else {
            return;
        };
        if started.restarted {
            println!("spark: the watchdog restarted the machine: the last boot stopped responding");
        }
        let every = u64::from(started.seconds) * SECOND / 3;
        self.pets = Some((every, now() + every));
        if self.hang && !started.restarted {
            println!("spark: not responding, as --test-hang asks, until the watchdog restarts");
            loop {
                core::hint::spin_loop();
            }
        }
    }

    /// Pets the watchdog if it is time to.
    fn pet_watchdog(&mut self, time: u64) {
        let (Some(power), Some((every, next))) = (self.power, self.pets) else { return };
        if time >= next {
            let _ = power::pet_watchdog(&SYS, power);
            self.pets = Some((every, time + every));
        }
    }

    /// Opens the control socket.
    pub fn listen(&mut self) {
        if let Err(err) = self.root.make_dir(CONTROL_DIR) {
            println!("spark: cannot make /{CONTROL_DIR}: {err}");
            return;
        }
        let Some(slot) = slots::take() else { return };
        let flags = PipeFlags::NON_BLOCKING.union(PipeFlags::CLOSE_ON_EXEC);
        let made =
            process::socket(&SYS, SLOT_PROCESS, slot, flags).map_err(|s| s.0).and_then(|()| {
                let path = alloc::format!("/{CONTROL}");
                libradix::abi::directory::bind_socket(
                    &SYS,
                    self.root.slot(),
                    CONTROL.as_bytes(),
                    slot,
                    path.as_bytes(),
                )
                .map_err(|s| s.0)?;
                file_abi::listen(&SYS, slot, CONNECTIONS_MAX as u32).map_err(|s| s.0)
            });
        match made {
            Ok(()) => match File::own(slot) {
                Ok(listener) => {
                    println!("spark: listening at /{CONTROL}");
                    self.listener = Some(listener);
                }
                Err(err) => println!("spark: cannot listen at /{CONTROL}: {err}"),
            },
            Err(status) => {
                let _ = capability::delete(&SYS, slot);
                slots::give_back(slot);
                println!("spark: cannot listen at /{CONTROL}: {status:#x}");
            }
        }
    }

    /// Reads the service files in `dir` and starts the enabled ones, in
    /// their order.
    pub fn start_services(&mut self, dir: &str) {
        let entries = match self.root.open_dir(dir, Rights::READ).and_then(|d| d.entries()) {
            Ok(entries) => entries,
            Err(err) => return println!("spark: cannot read /{dir}: {err}"),
        };
        let mut specs = Vec::new();
        for entry in entries {
            let Some(name) = entry.name.strip_suffix(".service") else { continue };
            if entry.kind != FileKind::File {
                continue;
            }
            let path = format!("{dir}/{}", entry.name);
            let text = match self.root.read_file(&path) {
                Ok(text) => String::from_utf8_lossy(&text).into_owned(),
                Err(err) => {
                    println!("spark: cannot read /{path}: {err}");
                    continue;
                }
            };
            match services::parse(name, &text) {
                Ok(spec) => specs.push(spec),
                Err(err) => println!("spark: /{path}: {err}; it is not started"),
            }
        }
        specs.sort_by(|a, b| (a.order, &a.name).cmp(&(b.order, &b.name)));
        let enabled = specs.iter().filter(|s| s.enabled).count();
        println!("spark: {} services in /{dir}, {enabled} enabled", specs.len());
        for spec in specs {
            let enabled = spec.enabled;
            self.services.push(Service {
                spec,
                state: State::Stopped,
                domain: None,
                output: None,
                line: Vec::new(),
                started_ns: 0,
                quick: 0,
                restarts: 0,
                last: None,
                held: false,
                restarting: false,
                stop_by: None,
                killed: false,
                oom_stops: 0,
            });
            if enabled {
                let index = self.services.len() - 1;
                self.start(index);
            }
        }
    }

    /// Hands over the program Spark was asked to run, to watch until it
    /// ends.
    pub fn watch_program(&mut self, program: Program) {
        non_blocking(program.master.slot());
        self.program = Some(program);
    }

    /// Says so when there is nothing to supervise but the control socket:
    /// no services, and no program.
    pub fn note_idle(&self) {
        if self.services.is_empty() && self.program.is_none() {
            println!("spark: nothing more to do; waiting");
        }
    }

    /// Starts service `index`.
    fn start(&mut self, index: usize) {
        let started = self.spawn(index);
        let service = &mut self.services[index];
        match started {
            Ok((number, output)) => {
                println!(
                    "spark: started {} ({}) as process {number}",
                    service.spec.name, service.spec.program
                );
                service.state = State::Running(number);
                service.output = Some(output);
                service.started_ns = now();
            }
            Err(what) => {
                println!("spark: could not start {}: {what}", service.spec.name);
                // As if it had ended at once.
                self.schedule(index, None);
            }
        }
    }

    /// Makes service `index`'s domain if it has none, and its process in
    /// it, running its program. Returns the process's number and the
    /// read end of its output.
    fn spawn(&mut self, index: usize) -> Result<(u64, File), String> {
        let root = self.root.slot();
        let services_domain = self.services_domain;
        let service = &mut self.services[index];
        let spec = &service.spec;
        if service.domain.is_none()
            && let Some(parent) = services_domain
        {
            let slot = slots::take().ok_or("no free slot")?;
            let limit = spec.memory_pages.unwrap_or(NO_LIMIT);
            let made = resource_domain::create(
                &SYS,
                parent,
                spec.name.as_bytes(),
                spec.weight,
                limit,
                spec.oom,
                slot,
            );
            match made {
                Ok(()) => service.domain = Some(slot),
                Err(status) => {
                    slots::give_back(slot);
                    return Err(format!("cannot make its domain: {:#x}", status.0));
                }
            }
        }
        let program = self
            .root
            .open(spec.program.trim_start_matches('/'), OpenFlags::NONE, Rights::READ)
            .map_err(|err| format!("{}: {err}", spec.program))?;
        let null = self
            .root
            .open("devices/null", OpenFlags::NONE, Rights::READ.union(Rights::WRITE))
            .map_err(|err| format!("/devices/null: {err}"))?;
        let (read, write) = (slots::take().ok_or("no free slot")?, slots::take());
        let Some(write) = write else {
            slots::give_back(read);
            return Err("no free slot".into());
        };
        if let Err(status) = process::pipe(&SYS, SLOT_PROCESS, read, write, PipeFlags::NONE) {
            slots::give_back(read);
            slots::give_back(write);
            return Err(format!("cannot make a pipe: {:#x}", status.0));
        }
        let finish = |slot: u32| {
            let _ = capability::delete(&SYS, slot);
            slots::give_back(slot);
        };
        let output = match File::own(read) {
            Ok(output) => output,
            Err(err) => {
                finish(write);
                return Err(format!("{err}"));
            }
        };
        let Some(child) = slots::take() else {
            finish(write);
            return Err("no free slot".into());
        };
        let made = match service.domain {
            Some(domain) => resource_domain::create_process(&SYS, domain, POSIX_SLOTS, child),
            None => process::create(&SYS, SLOT_PROCESS, ObjectKind::Process, POSIX_SLOTS, child),
        };
        if let Err(status) = made {
            finish(write);
            slots::give_back(child);
            return Err(format!("cannot make its process: {:#x}", status.0));
        }
        // The root, the random source and the log as Spark has them, with
        // the copy right that `fork` needs; /devices/null as its input and the
        // pipe as its output and errors.
        let shares = [
            (root, SLOT_ROOT),
            (SLOT_RANDOM, SLOT_RANDOM),
            (SLOT_LOG, SLOT_LOG),
            (null.slot(), SLOT_FD_BASE),
            (write, SLOT_FD_BASE + 1),
            (write, SLOT_FD_BASE + 2),
        ];
        let args: Vec<&str> = spec.arguments.iter().map(String::as_str).collect();
        let started = shares
            .iter()
            .try_for_each(|&(from, to)| {
                process::share(&SYS, child, from, to, Rights::ALL, 0)
                    .map_err(|status| format!("sharing slot {to} failed: {:#x}", status.0))
            })
            .and_then(|()| {
                let block = exec_block(&spec.program, &args, &spec.environment);
                process::exec(&SYS, child, program.slot(), &block)
                    .map_err(|status| format!("exec failed: {:#x}", status.0))
            });
        let number = process::info(&SYS, child).map_or(0, |info| info.number);
        // The child stays in the tree after its capability goes, for `wait`.
        finish(child);
        finish(write);
        started.map(|()| {
            non_blocking(read);
            (number, output)
        })
    }

    /// Service `index` ended with `status` (None: it could not start);
    /// starts it again later if its file says to, or a request did.
    fn schedule(&mut self, index: usize, status: Option<u32>) {
        let shutting_down = self.shutdown.is_some_and(|s| matches!(s, Shutdown::Stopping { .. }));
        let service = &mut self.services[index];
        let ran = now().saturating_sub(service.started_ns);
        service.last = status;
        let asked = core::mem::take(&mut service.restarting);
        let again = !shutting_down
            && !service.held
            && (asked
                || match service.spec.restart {
                    Restart::Always => true,
                    Restart::OnFailure => status.is_none_or(failed),
                    Restart::Never => false,
                });
        if !again {
            service.state = State::Stopped;
            return;
        }
        if status.is_some() && ran >= STEADY {
            service.quick = 0;
        }
        let wait = (BACKOFF_FIRST << service.quick.min(20)).min(BACKOFF_MAX);
        service.quick += 1;
        service.restarts += 1;
        service.state = State::Waiting(now() + wait);
        println!(
            "spark: starting {} again in {}.{} s",
            service.spec.name,
            wait / SECOND,
            wait % SECOND / (100 * MS)
        );
    }

    /// Copies what is in `output` to the log, a line at a time, each
    /// after `name: ` unless it starts with it. Returns false at its end.
    fn copy_lines(output: &File, line: &mut Vec<u8>, name: Option<&str>) -> bool {
        let mut buffer = [0u8; 512];
        loop {
            match file_abi::read(&SYS, output.slot(), file_abi::CURRENT, &mut buffer) {
                Ok(read) if read.count == 0 => {
                    flush_line(line, name);
                    return false;
                }
                Ok(read) => {
                    for &byte in &buffer[..read.count as usize] {
                        match byte {
                            b'\r' => {}
                            b'\n' => flush_line(line, name),
                            _ if line.len() < LINE_MAX => line.push(byte),
                            _ => {}
                        }
                    }
                }
                // Would block, or gone.
                Err(_) => return true,
            }
        }
    }

    /// Reaps the children that ended.
    fn reap(&mut self) {
        loop {
            let Ok(done) = process::wait(&SYS, SLOT_PROCESS, -1, ChildWaitFlags::NO_HANG) else {
                return;
            };
            if done.pid == 0 {
                return;
            }
            self.ended(done.pid, done.status);
        }
    }

    fn ended(&mut self, pid: u64, status: u32) {
        if self.program.as_ref().is_some_and(|p| p.number == pid) {
            let Some(mut program) = self.program.take() else { return };
            Self::copy_lines(&program.master, &mut program.line, None);
            flush_line(&mut program.line, None);
            println!("spark: process {pid} ({}) {}", program.path, ending(status));
            self.note_idle();
            return;
        }
        let Some(index) = self.services.iter().position(|s| s.state == State::Running(pid)) else {
            return;
        };
        let service = &mut self.services[index];
        if let Some(output) = service.output.take() {
            let name = service.spec.name.clone();
            Self::copy_lines(&output, &mut service.line, Some(&name));
            flush_line(&mut service.line, Some(&name));
        }
        let stops = service.domain.and_then(|d| resource_domain::info(&SYS, d).ok());
        let stops = stops.map_or(service.oom_stops, |info| info.oom_stops);
        let stopped = stops > service.oom_stops && status & 0x7f == signal::SIGKILL;
        service.oom_stops = stops;
        let cause = if stopped { " (stopped by the out-of-memory policy)" } else { "" };
        println!("spark: {} (process {pid}) {}{cause}", service.spec.name, ending(status));
        service.stop_by = None;
        if !self.drain(index, status) {
            self.schedule(index, Some(status));
        }
    }

    /// Service `index`'s process ended with `status`: stops what it left
    /// in its domain, if anything. Returns whether it waits for that to
    /// end before the service is started again.
    fn drain(&mut self, index: usize, status: u32) -> bool {
        let service = &mut self.services[index];
        let killed = core::mem::take(&mut service.killed);
        let Some(domain) = service.domain else { return false };
        let threads = resource_domain::info(&SYS, domain).map_or(0, |info| info.threads);
        if threads == 0 {
            return false;
        }
        if !killed {
            let plural = if threads == 1 { "" } else { "s" };
            println!(
                "spark: {} left {threads} thread{plural} behind; stopping its domain",
                service.spec.name
            );
            let _ = resource_domain::stop(&SYS, domain);
        }
        service.state = State::Draining(status, now() + DRAIN_POLL);
        true
    }

    /// Moves on the stops that are due: a service that did not end in
    /// STOP_WAIT has its domain stopped, and one whose domain has emptied
    /// is started again or not.
    fn step_stops(&mut self, time: u64) {
        for index in 0..self.services.len() {
            let service = &mut self.services[index];
            match service.state {
                State::Running(pid) if service.stop_by.is_some_and(|by| by <= time) => {
                    service.stop_by = None;
                    println!(
                        "spark: {} did not end in {} s; stopping its domain",
                        service.spec.name,
                        STOP_WAIT / SECOND
                    );
                    match service.domain {
                        Some(domain) => {
                            service.killed = true;
                            let _ = resource_domain::stop(&SYS, domain);
                        }
                        None => {
                            let _ = process::kill(&SYS, SLOT_PROCESS, pid as i64, signal::SIGKILL);
                        }
                    }
                }
                State::Draining(status, _) => {
                    let empty = service.domain.is_none_or(|d| {
                        resource_domain::info(&SYS, d).is_ok_and(|i| i.threads == 0)
                    });
                    if empty {
                        self.schedule(index, Some(status));
                    } else {
                        service.state = State::Draining(status, time + DRAIN_POLL);
                    }
                }
                _ => {}
            }
        }
    }

    /// Takes the connections that wait at the control socket.
    fn accept(&mut self) {
        let Some(listener) = &self.listener else { return };
        while self.connections.len() < CONNECTIONS_MAX {
            let Some(slot) = slots::take() else { return };
            if file_abi::accept(&SYS, listener.slot(), slot, PipeFlags::NON_BLOCKING).is_err() {
                slots::give_back(slot);
                return;
            }
            if let Ok(file) = File::own(slot) {
                let until = now() + CONNECTION_WAIT;
                self.connections.push(Connection { file, request: Vec::new(), until });
            }
        }
    }

    /// Reads what connection `index` sent; once it is a whole line,
    /// answers it and closes the connection.
    fn serve(&mut self, index: usize) {
        let mut buffer = [0u8; 256];
        let connection = &mut self.connections[index];
        let done =
            match file_abi::read(&SYS, connection.file.slot(), file_abi::CURRENT, &mut buffer) {
                Ok(read) if read.count == 0 => true,
                Ok(read) => {
                    connection.request.extend_from_slice(&buffer[..read.count as usize]);
                    connection.request.contains(&b'\n') || connection.request.len() > 1024
                }
                Err(_) => false,
            };
        if !done {
            return;
        }
        let connection = self.connections.remove(index);
        let request = String::from_utf8_lossy(&connection.request).into_owned();
        let request = request.lines().next().unwrap_or("").trim().to_string();
        let answer = self.answer(&request);
        let _ = file_abi::write(&SYS, connection.file.slot(), file_abi::CURRENT, answer.as_bytes());
    }

    /// The answer to the control request `request`.
    fn answer(&mut self, request: &str) -> String {
        let mut words = request.split_whitespace();
        let verb = words.next().unwrap_or("");
        let rest: Vec<&str> = words.collect();
        match verb {
            "poweroff" | "reboot" | "halt" => {
                let end = match verb {
                    "poweroff" => End::PowerOff,
                    "reboot" => End::Restart,
                    _ => End::Halt,
                };
                let force = rest.contains(&"--force");
                let delay = rest.iter().find_map(|w| w.parse::<u64>().ok()).unwrap_or(0);
                if force {
                    println!("spark: asked to {} the machine at once", end.words());
                    self.power(end);
                }
                // A delay of centuries waits for good, not round to soon.
                let at = now().saturating_add(delay.saturating_mul(SECOND));
                self.shutdown = Some(Shutdown::Pending { end, at });
                if delay == 0 {
                    println!("spark: asked to {} the machine", end.words());
                    format!("ok: going to {} now\n", end.words())
                } else {
                    println!("spark: the machine will {} in {delay} s", end.words());
                    format!("ok: going to {} in {delay} seconds\n", end.words())
                }
            }
            "cancel" => match self.shutdown {
                Some(Shutdown::Pending { end, .. }) => {
                    self.shutdown = None;
                    println!("spark: the {} was cancelled", end.words());
                    "ok: cancelled\n".into()
                }
                Some(Shutdown::Stopping { .. }) => "error: shutting down already\n".into(),
                None => "error: no shutdown is pending\n".into(),
            },
            "status" => {
                let mut text = String::new();
                for service in &self.services {
                    let (state, pid) = match service.state {
                        State::Running(pid) => ("running", pid.to_string()),
                        State::Waiting(_) => ("waiting", "-".into()),
                        State::Draining(..) => ("stopping", "-".into()),
                        State::Stopped if !service.spec.enabled => ("disabled", "-".into()),
                        State::Stopped => ("stopped", "-".into()),
                    };
                    let last = service.last.map_or("-".into(), |s| format!("{s:#x}"));
                    text.push_str(&format!(
                        "{}\t{state}\t{pid}\t{}\t{last}\t{}\t{}\n",
                        service.spec.name,
                        service.restarts,
                        service.spec.restart.name(),
                        service.spec.description
                    ));
                }
                if let Some(Shutdown::Pending { end, at }) = self.shutdown {
                    let left = at.saturating_sub(now()) / SECOND;
                    text.push_str(&format!("#\tshutdown\t{}\t{left}\n", end.words()));
                }
                text
            }
            "start" | "stop" | "restart" => {
                let Some(&name) = rest.first() else {
                    return format!("error: {verb} needs a service's name\n");
                };
                let Some(index) = self.services.iter().position(|s| s.spec.name == name) else {
                    return format!("error: no service is called {name}\n");
                };
                if self.shutdown.is_some_and(|s| matches!(s, Shutdown::Stopping { .. })) {
                    return "error: shutting down\n".into();
                }
                let state = self.services[index].state;
                if verb != "start"
                    && let State::Running(pid) = state
                {
                    // It ends, and `ended` starts it again unless stopped;
                    // `step_stops` stops its domain if it does not.
                    let service = &mut self.services[index];
                    service.held = verb == "stop";
                    service.restarting = verb == "restart";
                    service.quick = 0;
                    service.stop_by.get_or_insert(now() + STOP_WAIT);
                    let _ = process::kill(&SYS, SLOT_PROCESS, pid as i64, signal::SIGTERM);
                    println!("spark: asked to {verb} {name}");
                    return format!("ok: {verb}ping {name}\n");
                }
                if let State::Draining(..) = state {
                    // What it left is ending; then it starts again or not.
                    let service = &mut self.services[index];
                    service.held = verb == "stop";
                    service.restarting = verb != "stop";
                    println!("spark: asked to {verb} {name}");
                    return match verb {
                        "stop" => format!("ok: stopping {name}\n"),
                        _ => format!("ok: {name} starts once what it left has ended\n"),
                    };
                }
                if verb == "stop" {
                    self.services[index].state = State::Stopped;
                    self.services[index].held = true;
                    return format!("ok: {name} is not running\n");
                }
                self.services[index].held = false;
                if matches!(state, State::Running(_)) {
                    return format!("ok: {name} is running\n");
                }
                println!("spark: asked to start {name}");
                self.start(index);
                match self.services[index].state {
                    State::Running(pid) => format!("ok: started {name} as process {pid}\n"),
                    _ => format!("error: could not start {name}\n"),
                }
            }
            "" => "error: an empty request\n".into(),
            other => format!("error: unknown request `{other}`\n"),
        }
    }

    /// The power button was pressed: powers the machine off now, unless it
    /// is shutting down already. A shutdown set for later is moved to now,
    /// and a restart or halt becomes a power-off.
    fn button_pressed(&mut self) {
        if let Some(Shutdown::Stopping { end, .. }) = self.shutdown {
            println!("spark: the power button was pressed; going to {} already", end.words());
            return;
        }
        println!("spark: the power button was pressed; powering off the machine");
        self.shutdown = Some(Shutdown::Pending { end: End::PowerOff, at: now() });
    }

    /// Ends the machine with the power capability.
    fn power(&mut self, end: End) -> ! {
        if let Some(slot) = self.power {
            let done = match end {
                End::PowerOff => power::power_off(&SYS, slot),
                End::Restart => power::restart(&SYS, slot),
                End::Halt => power::halt(&SYS, slot),
            };
            if let Err(status) = done {
                println!(
                    "spark: could not {} the machine: {:#x}; halting it",
                    end.words(),
                    status.0
                );
                let _ = power::halt(&SYS, slot);
            }
        } else {
            println!("spark: the kernel gave no power capability; nothing can be turned off");
        }
        crate::idle()
    }

    /// The highest order among the services that run.
    fn running_order(&self) -> Option<u32> {
        self.services
            .iter()
            .filter(|s| matches!(s.state, State::Running(_)))
            .map(|s| s.spec.order)
            .max()
    }

    /// Asks the services of `order` to end.
    fn terminate(&mut self, order: u32) {
        for service in &self.services {
            if let State::Running(pid) = service.state
                && service.spec.order == order
            {
                let _ = process::kill(&SYS, SLOT_PROCESS, pid as i64, signal::SIGTERM);
            }
        }
    }

    /// Moves a shutdown on: starts it when it is due, and stops the
    /// services one group at a time, then ends the machine.
    fn step_shutdown(&mut self) {
        let time = now();
        match self.shutdown {
            Some(Shutdown::Pending { end, at }) if at <= time => {
                let running = self.services.iter().filter(|s| matches!(s.state, State::Running(_)));
                println!(
                    "spark: shutting down to {}: stopping {} services",
                    end.words(),
                    running.count()
                );
                for service in &mut self.services {
                    if let State::Waiting(_) = service.state {
                        service.state = State::Stopped;
                    }
                }
                if let Some(program) = &self.program {
                    let _ =
                        process::kill(&SYS, SLOT_PROCESS, program.number as i64, signal::SIGTERM);
                }
                self.next_group(end, time);
            }
            Some(Shutdown::Stopping { end, order, deadline, killed }) => {
                let left = self
                    .services
                    .iter()
                    .any(|s| matches!(s.state, State::Running(_)) && s.spec.order >= order);
                if !left {
                    return self.next_group(end, time);
                }
                if deadline > time {
                    return;
                }
                if killed {
                    // They are as good as gone.
                    for service in &mut self.services {
                        if matches!(service.state, State::Running(_)) && service.spec.order >= order
                        {
                            service.state = State::Stopped;
                        }
                    }
                    return self.next_group(end, time);
                }
                for service in &self.services {
                    if matches!(service.state, State::Running(_))
                        && service.spec.order >= order
                        && let Some(domain) = service.domain
                    {
                        println!("spark: {} did not end; stopping its domain", service.spec.name);
                        let _ = resource_domain::stop(&SYS, domain);
                    }
                }
                for service in &mut self.services {
                    if matches!(service.state, State::Running(_)) && service.spec.order >= order {
                        service.killed = true;
                    }
                }
                self.shutdown = Some(Shutdown::Stopping {
                    end,
                    order,
                    deadline: time + KILL_WAIT,
                    killed: true,
                });
            }
            _ => {}
        }
    }

    /// Stops the next group of services, or once none runs, what their
    /// processes left behind, and ends the machine.
    fn next_group(&mut self, end: End, time: u64) {
        match self.running_order() {
            Some(order) => {
                self.terminate(order);
                self.shutdown = Some(Shutdown::Stopping {
                    end,
                    order,
                    deadline: time + STOP_WAIT,
                    killed: false,
                });
            }
            None => {
                for service in &self.services {
                    if let Some(domain) = service.domain {
                        let _ = resource_domain::stop(&SYS, domain);
                    }
                }
                println!("spark: every service has stopped");
                self.power(end);
            }
        }
    }

    /// When the loop must look again without a wake-up: the next restart
    /// or shutdown step, or the watchdog's next pet.
    fn deadline(&self) -> Option<u64> {
        let restarts = self.services.iter().filter_map(|s| match s.state {
            State::Waiting(at) | State::Draining(_, at) => Some(at),
            State::Running(_) => s.stop_by,
            State::Stopped => None,
        });
        let shutdown = match self.shutdown {
            Some(Shutdown::Pending { at, .. }) => Some(at),
            Some(Shutdown::Stopping { deadline, .. }) => Some(deadline),
            None => None,
        };
        let connections = self.connections.iter().map(|c| c.until);
        let pet = self.pets.map(|(_, next)| next);
        restarts.chain(shutdown).chain(connections).chain(pet).min()
    }

    /// Supervises for good.
    pub fn run(&mut self) -> ! {
        self.start_watchdog();
        loop {
            // The children's notification, the power button's, the
            // control socket, the connections, the services' outputs and
            // the program's terminal, in that order.
            let mut entries: Vec<(PollEntry, Source)> = Vec::new();
            let readable = PollEvents::READABLE.0 as u16;
            let mut add = |slot: u32, source: Source| {
                entries.push((PollEntry { fd: slot as i32, events: readable, found: 0 }, source));
            };
            if let Some(slot) = self.children {
                add(slot, Source::Children);
            }
            if let Some(slot) = self.button {
                add(slot, Source::Button);
            }
            if let Some(listener) = &self.listener {
                add(listener.slot(), Source::Listener);
            }
            for (i, connection) in self.connections.iter().enumerate() {
                add(connection.file.slot(), Source::Connection(i));
            }
            for (i, service) in self.services.iter().enumerate() {
                if let Some(output) = &service.output {
                    add(output.slot(), Source::Output(i));
                }
            }
            if let Some(program) = &self.program
                && !program.hung_up
            {
                add(program.master.slot(), Source::Program);
            }
            let mut polled: Vec<PollEntry> = entries.iter().map(|(entry, _)| *entry).collect();
            let (timeout, flags) = match self.deadline() {
                Some(at) => (at.saturating_sub(now()), PollFlags::NONE),
                None => (0, PollFlags::FOREVER),
            };
            let address = polled.as_mut_ptr() as u64;
            let ready = process::poll(
                &SYS,
                SLOT_PROCESS,
                address,
                polled.len() as u32,
                0,
                timeout,
                0,
                flags,
            );
            if ready.is_ok_and(|r| r.ready > 0) {
                // Connections go from the end, so their indices stay right.
                for (entry, (_, source)) in polled.iter().zip(&entries).rev() {
                    if entry.found == 0 {
                        continue;
                    }
                    match *source {
                        Source::Children => {
                            if let Some(slot) = self.children {
                                let _ = syscalls::wait(&SYS, slot, WaitFlags::POLL, 0);
                            }
                        }
                        Source::Button => {
                            if let Some(slot) = self.button {
                                let _ = syscalls::wait(&SYS, slot, WaitFlags::POLL, 0);
                            }
                            self.button_pressed();
                        }
                        Source::Listener => self.accept(),
                        Source::Connection(i) => self.serve(i),
                        Source::Output(i) => {
                            let service = &mut self.services[i];
                            if let Some(output) = &service.output {
                                let name = service.spec.name.clone();
                                if !Self::copy_lines(output, &mut service.line, Some(&name)) {
                                    service.output = None;
                                }
                            }
                        }
                        Source::Program => {
                            if let Some(program) = &mut self.program {
                                let open =
                                    Self::copy_lines(&program.master, &mut program.line, None);
                                let hang_up = PollEvents::HANG_UP.0 as u16;
                                program.hung_up = !open || entry.found & hang_up != 0;
                            }
                        }
                    }
                }
            }
            self.reap();
            let time = now();
            // A connection that sent no whole request in time is closed;
            // its client reads the end.
            self.connections.retain(|c| c.until > time);
            self.step_stops(time);
            for index in 0..self.services.len() {
                if let State::Waiting(at) = self.services[index].state
                    && at <= time
                {
                    self.start(index);
                }
            }
            self.step_shutdown();
            self.pet_watchdog(time);
        }
    }
}

/// What a poll entry is for.
#[derive(Clone, Copy)]
enum Source {
    Children,
    Button,
    Listener,
    Connection(usize),
    Output(usize),
    Program,
}

/// Logs `line` after `name: `, unless it starts with that, and empties it.
fn flush_line(line: &mut Vec<u8>, name: Option<&str>) {
    if line.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(line);
    match name {
        Some(name) if !text.starts_with(name) || !text[name.len()..].starts_with(':') => {
            println!("{name}: {text}");
        }
        _ => println!("{text}"),
    }
    line.clear();
}
