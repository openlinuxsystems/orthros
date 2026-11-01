// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! netlog(8): sends the log to another computer over UDP as it is
//! written (ADR 0196), as Linux's netconsole does, for a machine whose
//! screen shows nothing or that hangs before anything else is saved.
//!
//! The log is the kernel's: its own records, the lines programs write to
//! it, and the lines of the services' output, which Spark copies there
//! (logd(8)). netlog reads `/devices/kmsg` from the boot's first record
//! the kernel still holds, and sends each record to the target as one
//! datagram, a line of text with its syslog(3) priority in front, as a
//! syslog server takes it, and its time since boot, as dmesg(1) shows it:
//!
//! ```text
//! <6>[    2.104511] Net: net0 is up, at 10.0.2.15/24
//! ```
//!
//! The target is `netlog=HOST:PORT` on the kernel command line, for one
//! boot, or else `target` in `/system/settings/netlog` (netlog(5)); the
//! port is 6666, netconsole's, unless it says. Without one netlog waits
//! for SIGHUP, which reads the settings again. A record that cannot be
//! sent yet, before the network is up, waits and is sent once it can be,
//! so a boot's first records reach the target too. Records go at most a
//! thousand a second, so that the hundreds the kernel holds at the start
//! do not overrun the listener's queue at once, as UDP drops what a full
//! queue cannot take.

use std::fs::{self, File};
use std::io::{self, Read};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::dmesg;
use crate::util::{self, Fail};

/// The settings.
pub const SETTINGS: &str = "/system/settings/netlog";
/// The kernel command line.
const CMDLINE: &str = "/kernel/cmdline";
/// The kernel log.
const KMSG: &str = "/devices/kmsg";
/// The port a target without one has: netconsole's.
pub const PORT: u16 = 6666;
/// The longest datagram sent: a record's text is cut to fit, as one that
/// big would be split on the way.
const DATAGRAM_MAX: usize = 1400;
/// How long a record that cannot be sent waits before it is tried again.
const RETRY: Duration = Duration::from_secs(1);
/// The least time between two records sent: a thousand a second.
const PACE: Duration = Duration::from_millis(1);

static STOP: AtomicBool = AtomicBool::new(false);
static RELOAD: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

extern "C" fn on_hangup(_: libc::c_int) {
    RELOAD.store(true, Ordering::Relaxed);
}

/// The settings: where to send the log, and the least urgent level sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// `HOST:PORT`, `HOST`, `[ADDRESS]:PORT` or an address.
    pub target: Option<String>,
    /// Only records of this level or a more urgent one (syslog(3)'s
    /// numbers, 0 to 7); all of them without it.
    pub level: Option<u8>,
}

/// Reads `/system/settings/netlog`'s `key = value` lines, and the lines it
/// could not read.
pub fn parse_settings(text: &str) -> (Settings, Vec<String>) {
    let mut settings = Settings::default();
    let mut wrong = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        match line.split_once('=').map(|(k, v)| (k.trim(), v.trim())) {
            Some(("target", "")) => settings.target = None,
            Some(("target", value)) => settings.target = Some(value.to_string()),
            Some(("level", value)) => match value.parse::<u8>() {
                Ok(level) if level < 8 => settings.level = Some(level),
                _ => wrong.push(line.to_string()),
            },
            _ => wrong.push(line.to_string()),
        }
    }
    (settings, wrong)
}

/// The target `netlog=` names on the kernel command line `cmdline`.
pub fn cmdline_target(cmdline: &str) -> Option<String> {
    cmdline
        .split_whitespace()
        .take_while(|word| *word != "--")
        .filter_map(|word| word.strip_prefix("netlog="))
        .last()
        .filter(|target| !target.is_empty())
        .map(String::from)
}

/// The host and port of `target`, with [`PORT`] for one without.
pub fn host_port(target: &str) -> (String, u16) {
    if let Some(rest) = target.strip_prefix('[')
        && let Some((host, port)) = rest.split_once(']')
    {
        let port = port.strip_prefix(':').and_then(|p| p.parse().ok()).unwrap_or(PORT);
        return (host.to_string(), port);
    }
    match target.rsplit_once(':') {
        // One colon: a host and its port; more: an IPv6 address alone.
        Some((host, port)) if !host.contains(':') => {
            (host.to_string(), port.parse().unwrap_or(PORT))
        }
        _ => (target.to_string(), PORT),
    }
}

/// The datagram for a record: `<PRIORITY>[SECONDS.MICROSECONDS] TEXT`, cut
/// to [`DATAGRAM_MAX`] bytes at a character's end.
pub fn datagram(line: &dmesg::Line) -> Vec<u8> {
    let priority = u32::from(line.facility) * 8 + u32::from(line.level);
    let mut text = format!(
        "<{priority}>[{:5}.{:06}] {}",
        line.usec / 1_000_000,
        line.usec % 1_000_000,
        line.text
    );
    if text.len() > DATAGRAM_MAX {
        let mut end = DATAGRAM_MAX;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text.push('\n');
    text.into_bytes()
}

/// What netlog sends to, with the settings it came from.
struct Target {
    text: String,
    level: Option<u8>,
}

/// The target and level, from the kernel command line or the settings.
fn target() -> Option<Target> {
    let (settings, wrong) = match fs::read_to_string(SETTINGS) {
        Ok(text) => parse_settings(&text),
        Err(_) => (Settings::default(), Vec::new()),
    };
    for line in wrong {
        eprintln!("netlog: {SETTINGS}: not a setting: {line}");
    }
    let from_kernel = fs::read_to_string(CMDLINE).ok().and_then(|cmdline| cmdline_target(&cmdline));
    let text = from_kernel.or(settings.target)?;
    Some(Target { text, level: settings.level })
}

/// A socket connected to `target`, whose name is looked up again each
/// try; `None` once netlog is to stop or read its settings again.
fn connect(target: &str) -> Option<UdpSocket> {
    let (host, port) = host_port(target);
    let mut said = false;
    while !STOP.load(Ordering::Relaxed) && !RELOAD.load(Ordering::Relaxed) {
        let tried = (host.as_str(), port).to_socket_addrs().and_then(|mut addresses| {
            let address: SocketAddr = addresses
                .next()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no address"))?;
            let any: SocketAddr =
                if address.is_ipv4() { ([0u8; 4], 0).into() } else { ([0u16; 8], 0).into() };
            let socket = UdpSocket::bind(any)?;
            socket.connect(address)?;
            Ok((socket, address))
        });
        match tried {
            Ok((socket, address)) => {
                println!("netlog: sending the log to {address}");
                return Some(socket);
            }
            Err(e) if !said => {
                println!(
                    "netlog: {target}: {}; trying again each second",
                    util::strip_os_error(&e)
                );
                said = true;
            }
            Err(_) => {}
        }
        thread::sleep(RETRY);
    }
    None
}

/// Sends `data`, and waits and sends it again while the network cannot
/// take it yet. False once netlog is to stop or read its settings again.
fn send(socket: &UdpSocket, data: &[u8]) -> bool {
    loop {
        if STOP.load(Ordering::Relaxed) || RELOAD.load(Ordering::Relaxed) {
            return false;
        }
        match socket.send(data) {
            Ok(_) => return true,
            // The target's port is closed: it was told; the record is gone.
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => return true,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // No route, no address yet: the network is not up.
            Err(_) => thread::sleep(RETRY),
        }
    }
}

/// Sends every record from the first the kernel holds, then each as it
/// comes, until netlog is to stop or read its settings again.
fn stream(socket: &UdpSocket, level: Option<u8>) -> Result<(), Fail> {
    let mut kmsg = File::open(KMSG).map_err(|e| util::failed(&KMSG, e))?;
    let mut buffer = vec![0u8; 8192];
    let mut last_sent: Option<Instant> = None;
    while !STOP.load(Ordering::Relaxed) && !RELOAD.load(Ordering::Relaxed) {
        let n = match kmsg.read(&mut buffer) {
            Ok(n) => n,
            // Records were lost to newer ones before they were read.
            Err(e) if e.raw_os_error() == Some(libc::EPIPE) => continue,
            // A signal: look at what it asked.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(util::failed(&KMSG, e)),
        };
        if n == 0 {
            break;
        }
        let Some(line) = dmesg::parse(&buffer[..n]) else { continue };
        if level.is_some_and(|level| line.level > level) {
            continue;
        }
        if let Some(sent) = last_sent {
            let wait = PACE.saturating_sub(sent.elapsed());
            if !wait.is_zero() {
                thread::sleep(wait);
            }
        }
        if !send(socket, &datagram(&line)) {
            break;
        }
        last_sent = Some(Instant::now());
    }
    Ok(())
}

/// Starts netlog.
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    if let Some(arg) = args.next()? {
        return Err(args.unknown(&arg));
    }
    for (signal, handler) in
        [(libc::SIGTERM, on_term as extern "C" fn(libc::c_int)), (libc::SIGHUP, on_hangup)]
    {
        // SAFETY: all zeros is a valid sigaction: numbers, a signal set
        // and a function address. No SA_RESTART: a read of the log ends
        // with EINTR, so the loop sees the signal.
        let mut action: libc::sigaction = unsafe { util::zeroed() };
        action.sa_sigaction = handler as *const () as usize;
        // SAFETY: the handlers only store to an atomic.
        unsafe { libc::sigaction(signal, &raw const action, std::ptr::null_mut()) };
    }
    let mut idle_said = false;
    while !STOP.load(Ordering::Relaxed) {
        RELOAD.store(false, Ordering::Relaxed);
        let Some(target) = target() else {
            if !idle_said {
                println!(
                    "netlog: no target: netlog=HOST:PORT on the kernel command line, or target \
                     in {SETTINGS}, names one"
                );
                idle_said = true;
            }
            // SAFETY: pause(2) waits for a signal and touches no memory.
            unsafe { libc::pause() };
            continue;
        };
        idle_said = false;
        if let Some(socket) = connect(&target.text) {
            stream(&socket, target.level)?;
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_read() {
        let (settings, wrong) =
            parse_settings("# ours\ntarget = 10.0.2.2:5140\nlevel = 6\nloud = yes\nlevel = 9\n");
        assert_eq!(settings.target.as_deref(), Some("10.0.2.2:5140"));
        assert_eq!(settings.level, Some(6));
        assert_eq!(wrong, ["loud = yes", "level = 9"]);
        assert_eq!(
            cmdline_target("quiet netlog=logs.example:514 -- --services").as_deref(),
            Some("logs.example:514")
        );
        assert_eq!(cmdline_target("quiet -- netlog=x"), None);
        assert_eq!(host_port("logs.example:514"), ("logs.example".into(), 514));
        assert_eq!(host_port("logs.example"), ("logs.example".into(), PORT));
        assert_eq!(host_port("[fd00::1]:7"), ("fd00::1".into(), 7));
        assert_eq!(host_port("fd00::1"), ("fd00::1".into(), PORT));
    }

    #[test]
    fn records_become_lines() {
        let line = dmesg::parse(b"14,7,2104511,-,process=1;spark: started\n").unwrap();
        assert_eq!(datagram(&line), b"<14>[    2.104511] spark: started\n");
        let long = dmesg::parse(format!("6,1,0,-;{}", "\u{e9}".repeat(1000)).as_bytes()).unwrap();
        let sent = datagram(&long);
        assert!(sent.len() <= DATAGRAM_MAX + 1);
        assert!(std::str::from_utf8(&sent).is_ok());
    }
}
