// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! timed(8): keeps the clock right from the network (ADR 0194).
//!
//! It asks the time servers `/system/settings/timed` names (timed(5)),
//! or Cloudflare's, Netnod's and PTB's, over Network Time Security (RFC
//! 8915): a TLS connection to the server's key establishment port, its
//! certificate checked, gives the keys and cookies with which each NTP
//! request and answer is authenticated, so that nobody on the way can
//! change the time it is told. Only while no NTS server answers does it
//! ask the plain NTP servers of `fallback` (SNTP, RFC 4330), and says so.
//!
//! Each round takes four samples of the first server that answers and
//! keeps the one with the shortest round trip, as NTP's clock filter
//! does. An offset past 128 ms steps the clock (radix-clock(2)'s `set`),
//! and a smaller one slews it (`slew`), so that it never jumps or runs
//! backwards for small corrections. The real-time clock is written after
//! a step and every eleven minutes, as Linux's eleven-minute mode does.
//! Rounds start every 64 seconds and grow to every 17 minutes while the
//! clock stays close, and come again in 32 seconds after a failure.
//!
//! A clock far from right fails certificate checks: an RTC with a dead
//! battery says 2000. At its first sync after boot timed sets a clock
//! behind the time it last synced (kept in `/system/state/timed`) forward
//! to that; and if a server's certificate is not valid by the clock, it
//! asks the same server for the time over plain NTP once, checks the
//! certificate at that time instead, unless it is before the last sync,
//! and if it passes, goes on over NTS.
//!
//! What it did last goes to `/system/live/timed/status`, which
//! timedatectl(1) shows. SIGHUP reads the settings again; SIGTERM ends it.

use std::fs;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use radix_abi::clock;
use radix_abi::startup::SLOT_CLOCK;
use ring::rand::{SecureRandom, SystemRandom};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::crypto::ring as provider;
use rustls::pki_types::UnixTime;
use rustls::time_provider::TimeProvider;
use rustls::{CertificateError, ClientConfig, RootCertStore};

use crate::args::Args;
use crate::sys::SYS;
use crate::tls;
use crate::util::{self, Fail};
use netproto::ntp::{self, KEY_LEN};

/// Its settings (timed(5)), what it did last, and where it keeps when it
/// last synced.
pub const SETTINGS: &str = "/system/settings/timed";
pub const STATUS: &str = "/system/live/timed/status";
/// Its process number, while it runs, for timedatectl(1).
pub const PID: &str = "/system/live/timed/pid";
const LAST: &str = "/system/state/timed/last";

/// The NTS servers asked unless the settings name others: three run by
/// different organizations in different countries.
pub const DEFAULT_SERVERS: [&str; 3] = ["time.cloudflare.com", "nts.netnod.se", "ptbtime1.ptb.de"];
/// The plain NTP servers asked only while no NTS server answers.
pub const DEFAULT_FALLBACK: [&str; 1] = ["pool.ntp.org"];

/// How long a server has to answer, or a connection to be made.
const TIMEOUT: Duration = Duration::from_secs(3);
/// The samples a round takes of a server.
const SAMPLES: usize = 4;
/// The shortest and longest times between rounds, and after a failure.
const POLL_MIN: Duration = Duration::from_secs(64);
const POLL_MAX: Duration = Duration::from_secs(1024);
const RETRY: Duration = Duration::from_secs(32);
/// How often the real-time clock is written.
const RTC_EVERY: Duration = Duration::from_mins(11);
/// An offset past this is stepped, not slewed: radix-clock(2)'s
/// SLEW_MAX_NS.
const STEP_NS: i128 = clock::SLEW_MAX_NS as i128;
/// An offset under this is left for the next round.
const CLOSE_NS: i128 = 500_000;
/// The cookies kept for a server: RFC 8915 has servers give eight.
const COOKIES: usize = 8;
/// A sample is not taken from a server whose root distance, half its
/// root delay plus its root dispersion, is past this (NTP's MAXDIST).
const MAX_DISTANCE_NS: i128 = 1_500_000_000;

static STOP: AtomicBool = AtomicBool::new(false);
static RELOAD: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

extern "C" fn on_hangup(_: libc::c_int) {
    RELOAD.store(true, Ordering::Relaxed);
}

/// What timed(5) says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// NTS servers, as `name[:port]` of their key establishment.
    pub servers: Vec<String>,
    /// Plain NTP servers, as `name[:port]`.
    pub fallback: Vec<String>,
    /// More authorities' certificates to trust, PEM files.
    pub certificates: Vec<String>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            servers: DEFAULT_SERVERS.map(String::from).to_vec(),
            fallback: DEFAULT_FALLBACK.map(String::from).to_vec(),
            certificates: Vec::new(),
        }
    }
}

/// Reads timed(5)'s `text`: the settings, and what is wrong with any
/// line, which is left out. `servers =` and `fallback =` with nothing
/// after them ask none.
pub fn parse_settings(text: &str) -> (Settings, Vec<String>) {
    let mut settings = Settings::default();
    let mut wrong = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            wrong.push(format!("{SETTINGS}:{}: no '=' in '{line}'", number + 1));
            continue;
        };
        let words = value.split_whitespace().map(String::from).collect();
        match key.trim() {
            "servers" => settings.servers = words,
            "fallback" => settings.fallback = words,
            "certificates" => settings.certificates = words,
            other => wrong.push(format!("{SETTINGS}:{}: no setting '{other}'", number + 1)),
        }
    }
    (settings, wrong)
}

/// Splits `name[:port]`, or `[v6]:port`.
fn host_port(text: &str, default: u16) -> (String, u16) {
    if let Some(rest) = text.strip_prefix('[')
        && let Some((host, after)) = rest.split_once(']')
    {
        let port = after.strip_prefix(':').and_then(|p| p.parse().ok()).unwrap_or(default);
        return (host.to_string(), port);
    }
    match text.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => {
            (host.to_string(), port.parse().unwrap_or(default))
        }
        _ => (text.to_string(), default),
    }
}

/// One sample of a server's time.
#[derive(Clone, Copy, Debug)]
struct Sample {
    /// How far the local clock is behind the server's, and the round
    /// trip, in nanoseconds.
    offset: i128,
    delay: i128,
    stratum: u8,
}

/// An NTS session with a server: its keys, the cookies it gave, and where
/// its NTP server is.
struct Session {
    c2s: [u8; KEY_LEN],
    s2c: [u8; KEY_LEN],
    cookies: Vec<Vec<u8>>,
    ntp: SocketAddr,
}

/// A clock the time checks of certificates use: the system's, or one
/// told the time.
#[derive(Debug)]
struct Told(Option<u64>);

impl TimeProvider for Told {
    fn current_time(&self) -> Option<UnixTime> {
        match self.0 {
            Some(seconds) => Some(UnixTime::since_unix_epoch(Duration::from_secs(seconds))),
            None => Some(UnixTime::now()),
        }
    }
}

/// timed's state.
struct Timed {
    settings: Settings,
    roots: Arc<RootCertStore>,
    random: SystemRandom,
    /// The NTS sessions, one for each of the settings' servers.
    sessions: Vec<Option<Session>>,
    /// Whether it set the clock since it started, and when it last wrote
    /// the real-time clock.
    synced: bool,
    rtc_written: Option<Instant>,
    /// What the status file says.
    status: String,
}

fn now_ns() -> i128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i128)
}

/// Whether `err` is a certificate's time check failing.
fn certificate_time(err: &io::Error) -> bool {
    let inner = err.get_ref().and_then(|e| e.downcast_ref::<rustls::Error>());
    matches!(
        inner,
        Some(rustls::Error::InvalidCertificate(
            CertificateError::Expired
                | CertificateError::NotValidYet
                | CertificateError::ExpiredContext { .. }
                | CertificateError::NotValidYetContext { .. }
        ))
    )
}

impl Timed {
    fn random<const N: usize>(&self) -> [u8; N] {
        let mut bytes = [0; N];
        let _ = self.random.fill(&mut bytes);
        bytes
    }

    /// The settings' TLS client, checking certificates by `time`.
    fn config(&self, time: Option<u64>) -> io::Result<Arc<ClientConfig>> {
        let provider = Arc::new(provider::default_provider());
        let verifier =
            WebPkiServerVerifier::builder_with_provider(self.roots.clone(), provider.clone())
                .build()
                .map_err(io::Error::other)?;
        let verifier: Arc<dyn ServerCertVerifier> = verifier;
        let mut config = ClientConfig::builder_with_details(provider, Arc::new(Told(time)))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(io::Error::other)?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        config.alpn_protocols = vec![ntp::NTS_ALPN.to_vec()];
        Ok(Arc::new(config))
    }

    /// NTS key establishment with `server` (`name[:port]`): keys, cookies
    /// and where to send NTP. If the certificate is not valid by the
    /// clock before the first sync, the server's plain NTP time is asked
    /// for, and the certificate checked by it.
    fn establish(&self, server: &str) -> Result<Session, String> {
        let (host, port) = host_port(server, ntp::NTS_KE_PORT);
        let address = (host.as_str(), port)
            .to_socket_addrs()
            .map_err(|e| format!("cannot look {host} up: {}", util::strip_os_error(&e)))?
            .next()
            .ok_or_else(|| format!("{host} has no address"))?;
        let name = tls::server_name(
            host.parse::<std::net::IpAddr>().is_err().then_some(host.as_str()),
            address,
        )
        .map_err(|e| e.to_string())?;
        let config = self.config(None).map_err(|e| e.to_string())?;
        let stream = match tls::connect(address, name.clone(), &config, TIMEOUT) {
            Err(err) if certificate_time(&err) && !self.synced => {
                let ntp = SocketAddr::new(address.ip(), ntp::NTP_PORT);
                let sample = self.sntp_sample(ntp).map_err(|e| {
                    format!(
                        "its certificate is not valid by the clock, and it gave no plain time: {e}"
                    )
                })?;
                let told = (now_ns() + sample.offset) / 1_000_000_000;
                // An unauthenticated time before the last sync could make
                // a certificate that expired since valid again.
                if let Some(last) = Self::last_synced()
                    && told < i128::from(last)
                {
                    return Err(
                        "its certificate is not valid by the clock, and its plain time is before \
                         timed last synced"
                            .into(),
                    );
                }
                eprintln!(
                    "timed: {host}'s certificate is not valid by the clock; checking it by the time its NTP gives"
                );
                let config = self.config(u64::try_from(told).ok()).map_err(|e| e.to_string())?;
                tls::connect(address, name, &config, TIMEOUT)
            }
            other => other,
        };
        let mut stream = stream
            .map_err(|e| format!("NTS key establishment failed: {}", util::strip_os_error(&e)))?;
        stream
            .write_all(&ntp::ke_request())
            .and_then(|()| stream.flush())
            .map_err(|e| e.to_string())?;
        let mut data = Vec::new();
        let mut buf = [0; 4096];
        let (records, _) = loop {
            if let Some(found) = ntp::ke_records(&data) {
                break found;
            }
            match stream.read(&mut buf) {
                Ok(0) => return Err("the server closed before its answer ended".into()),
                Ok(n) if data.len() + n <= 65_536 => data.extend_from_slice(&buf[..n]),
                Ok(_) => return Err("too long an answer".into()),
                Err(err) => {
                    return Err(format!("reading its answer: {}", util::strip_os_error(&err)));
                }
            }
        };
        let established = ntp::ke_answer(&records)?;
        let mut c2s = [0; KEY_LEN];
        let mut s2c = [0; KEY_LEN];
        let exported = stream
            .conn
            .export_keying_material(
                &mut c2s,
                ntp::EXPORTER_LABEL,
                Some(&ntp::exporter_context(false)),
            )
            .and_then(|_| {
                stream.conn.export_keying_material(
                    &mut s2c,
                    ntp::EXPORTER_LABEL,
                    Some(&ntp::exporter_context(true)),
                )
            });
        exported.map_err(|e| format!("no keys from the TLS session: {e}"))?;
        let ntp_host = established.server.clone().unwrap_or(host);
        let ntp_port = established.port.unwrap_or(ntp::NTP_PORT);
        let ntp = (ntp_host.as_str(), ntp_port)
            .to_socket_addrs()
            .ok()
            .and_then(|mut addresses| addresses.next())
            .ok_or_else(|| format!("its NTP server {ntp_host} has no address"))?;
        stream.conn.send_close_notify();
        let _ = stream.flush();
        Ok(Session { c2s, s2c, cookies: established.cookies, ntp })
    }

    /// One NTS-protected sample over `session`, which it takes a cookie
    /// from and gives those the answer carries.
    fn nts_sample(&self, session: &mut Session) -> Result<Sample, String> {
        let cookie = session.cookies.pop().ok_or("no cookies left")?;
        let unique = self.random::<32>();
        let nonce = self.random::<16>();
        let transmit = u64::from_ne_bytes(self.random::<8>());
        let wanted = COOKIES.saturating_sub(session.cookies.len() + 1);
        let request = ntp::nts_request(transmit, &unique, &cookie, wanted, &session.c2s, &nonce)
            .ok_or("cannot seal the request")?;
        let (packet, t1, t4) = exchange(session.ntp, &request, transmit)?;
        let header = ntp::answer(&packet).ok_or("not an NTP answer")?;
        if header.stratum == 0 && &header.reference == b"NTSN" {
            session.cookies.clear();
            return Err("the server no longer takes its cookies (NTSN)".into());
        }
        let cookies = ntp::nts_answer(&packet, &unique, &session.s2c)?;
        session.cookies.extend(cookies);
        session.cookies.truncate(COOKIES);
        sample_of(&packet, t1, t4)
    }

    /// One plain NTP sample of `address`.
    fn sntp_sample(&self, address: SocketAddr) -> Result<Sample, String> {
        let transmit = u64::from_ne_bytes(self.random::<8>());
        let (packet, t1, t4) = exchange(address, &ntp::request(transmit), transmit)?;
        sample_of(&packet, t1, t4)
    }

    /// The best of SAMPLES samples from `take`: the one with the shortest
    /// round trip.
    fn best(mut take: impl FnMut() -> Result<Sample, String>) -> Result<Sample, String> {
        let mut best: Option<Sample> = None;
        let mut last_error = String::new();
        for _ in 0..SAMPLES {
            match take() {
                Ok(sample) => {
                    if best.is_none_or(|b| sample.delay < b.delay) {
                        best = Some(sample);
                    }
                }
                Err(why) => last_error = why,
            }
        }
        best.ok_or(last_error)
    }

    /// A round: a sample from the first NTS server that gives one, or
    /// else a plain NTP server; what it is from, and how it went.
    fn round(&mut self) -> Result<(Sample, String), Vec<String>> {
        let mut why = Vec::new();
        let servers = self.settings.servers.clone();
        self.sessions.resize_with(servers.len(), || None);
        for (index, server) in servers.iter().enumerate() {
            if self.sessions[index].as_ref().is_none_or(|s| s.cookies.is_empty()) {
                match self.establish(server) {
                    Ok(session) => self.sessions[index] = Some(session),
                    Err(err) => {
                        why.push(format!("{server}: {err}"));
                        continue;
                    }
                }
            }
            let Some(mut session) = self.sessions[index].take() else { continue };
            let result = Self::best(|| self.nts_sample(&mut session));
            self.sessions[index] = Some(session);
            match result {
                Ok(sample) => return Ok((sample, format!("{server} over NTS"))),
                Err(err) => {
                    // A new session next time.
                    self.sessions[index] = None;
                    why.push(format!("{server}: {err}"));
                }
            }
        }
        for server in &self.settings.fallback {
            let (host, port) = host_port(server, ntp::NTP_PORT);
            let addresses: Vec<SocketAddr> =
                (host.as_str(), port).to_socket_addrs().map_or_default(Iterator::collect);
            if addresses.is_empty() {
                why.push(format!("{server}: cannot look it up"));
            }
            for address in addresses.into_iter().take(4) {
                match Self::best(|| self.sntp_sample(address)) {
                    Ok(sample) => {
                        eprintln!(
                            "timed: no NTS server answered ({}); taking the time from {server} ({address}) over plain NTP, which is not authenticated",
                            why.join("; ")
                        );
                        return Ok((sample, format!("{server} ({address}) over plain NTP")));
                    }
                    Err(err) => why.push(format!("{server} ({address}): {err}")),
                }
            }
        }
        Err(why)
    }

    /// Sets the clock by `sample`: steps it past STEP_NS, else slews it.
    /// Returns what it did.
    fn apply(&mut self, sample: &Sample) -> Result<String, String> {
        let offset = sample.offset;
        let ms = offset as f64 / 1e6;
        let did = if offset.abs() > STEP_NS {
            let target = u64::try_from(now_ns() + offset).map_err(|_| "a time before 1970")?;
            clock::set(&SYS, SLOT_CLOCK, target)
                .map_err(|s| format!("cannot set the clock: {:#x}", s.0))?;
            self.rtc_written = Some(Instant::now());
            format!("stepped the clock {ms:+.3} ms")
        } else if offset.abs() > CLOSE_NS {
            let delta = i64::try_from(offset).unwrap_or(0);
            clock::slew(&SYS, SLOT_CLOCK, delta)
                .map_err(|s| format!("cannot slew the clock: {:#x}", s.0))?;
            format!("slewing the clock {ms:+.3} ms")
        } else {
            format!("the clock is {ms:+.3} ms off; left as it is")
        };
        self.synced = true;
        if self.rtc_written.is_none_or(|at| at.elapsed() >= RTC_EVERY) {
            match clock::write_rtc(&SYS, SLOT_CLOCK) {
                Ok(()) => self.rtc_written = Some(Instant::now()),
                Err(status) => {
                    eprintln!("timed: cannot write the real-time clock: {:#x}", status.0);
                }
            }
        }
        let seconds = (now_ns() / 1_000_000_000).to_string();
        let _ = write_file(LAST, &(seconds + "\n"));
        Ok(did)
    }

    /// When timed last synced, in seconds since 1970, as kept from boot to
    /// boot.
    fn last_synced() -> Option<u64> {
        fs::read_to_string(LAST).ok().and_then(|t| t.trim().parse::<u64>().ok())
    }

    /// Sets a clock behind the time timed last synced forward to it.
    fn not_before_last() {
        let Some(last) = Self::last_synced() else {
            return;
        };
        let now = now_ns() / 1_000_000_000;
        if i128::from(last) > now {
            eprintln!(
                "timed: the clock says {}, before it last synced, {}; setting it to that",
                util::time_text(i64::try_from(now).unwrap_or(0), "%F %T UTC", true),
                util::time_text(i64::try_from(last).unwrap_or(0), "%F %T UTC", true),
            );
            let _ = clock::set(&SYS, SLOT_CLOCK, last.saturating_mul(1_000_000_000));
        }
    }

    fn read_settings(&mut self) {
        let (settings, wrong) = match fs::read_to_string(SETTINGS) {
            Ok(text) => parse_settings(&text),
            Err(_) => (Settings::default(), Vec::new()),
        };
        for line in wrong {
            eprintln!("timed: {line}");
        }
        if settings != self.settings || self.roots.is_empty() {
            self.roots = Arc::new(tls::roots(&settings.certificates, "timed"));
            self.sessions.clear();
        }
        println!(
            "timed: NTS servers {}; plain NTP only while none answers: {}",
            if settings.servers.is_empty() { "none".into() } else { settings.servers.join(" ") },
            if settings.fallback.is_empty() { "none".into() } else { settings.fallback.join(" ") },
        );
        self.settings = settings;
    }

    fn write_status(&mut self, text: String) {
        if text != self.status && write_file(STATUS, &text) {
            self.status = text;
        }
    }
}

/// Sends `request` to `address` and waits for its answer, whose origin
/// timestamp must be `transmit`: the answer, and the local times it went
/// and came back.
fn exchange(
    address: SocketAddr,
    request: &[u8],
    transmit: u64,
) -> Result<(Vec<u8>, i128, i128), String> {
    let unspecified: SocketAddr =
        if address.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }.parse().map_err(|_| "no address")?;
    let socket = UdpSocket::bind(unspecified).map_err(|e| util::strip_os_error(&e))?;
    socket.connect(address).map_err(|e| util::strip_os_error(&e))?;
    let deadline = Instant::now() + TIMEOUT;
    let t1 = now_ns();
    socket.send(request).map_err(|e| util::strip_os_error(&e))?;
    let mut buf = [0; 2048];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("no answer".into());
        }
        let _ = socket.set_read_timeout(Some(left));
        let len = match socket.recv(&mut buf) {
            Ok(len) => len,
            Err(err)
                if matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
            {
                return Err("no answer".into());
            }
            Err(err) => return Err(util::strip_os_error(&err)),
        };
        let t4 = now_ns();
        // Anything else is not the answer: a stray or forged datagram.
        if ntp::answer(&buf[..len]).is_some_and(|a| a.origin == transmit && a.mode == 4) {
            return Ok((buf[..len].to_vec(), t1, t4));
        }
    }
}

/// The sample an answer gives, sent at `t1` and come back at `t4`, or why
/// it gives none.
fn sample_of(packet: &[u8], t1: i128, t4: i128) -> Result<Sample, String> {
    let header = ntp::answer(packet).ok_or("not an NTP answer")?;
    if header.stratum == 0 {
        let code = String::from_utf8_lossy(&header.reference).into_owned();
        return Err(format!("the server said {code} (kiss of death)"));
    }
    if header.stratum >= 16 || header.leap == 3 {
        return Err("the server's own clock is not synchronized".into());
    }
    if header.transmit == 0 || header.receive == 0 {
        return Err("the answer has no times".into());
    }
    let near = u64::try_from(t1).unwrap_or(0);
    let t2 = ntp::unix_ns(header.receive, near);
    let t3 = ntp::unix_ns(header.transmit, near);
    let (offset, delay) = ntp::offset_delay(t1, t2, t3, t4);
    // Short NTP format: 16.16 seconds.
    let short = |value: u32| (i128::from(value) * 1_000_000_000) >> 16;
    if short(header.root_delay) / 2 + short(header.root_dispersion) > MAX_DISTANCE_NS {
        return Err("the server is too far from its own reference".into());
    }
    Ok(Sample { offset, delay: delay.max(0), stratum: header.stratum })
}

/// Writes `text` into `path` whole, through a new file renamed over it.
fn write_file(path: &str, text: &str) -> bool {
    let new = format!("{path}.new");
    let written = Path::new(path)
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| fs::write(&new, text))
        .and_then(|()| fs::rename(&new, path));
    if let Err(err) = &written {
        eprintln!("timed: cannot write {path}: {}", util::strip_os_error(err));
    }
    written.is_ok()
}

/// Sleeps for `time`, or until a signal asks for something.
fn sleep(time: Duration) {
    let until = Instant::now() + time;
    while Instant::now() < until && !STOP.load(Ordering::Relaxed) && !RELOAD.load(Ordering::Relaxed)
    {
        thread::sleep(Duration::from_secs(1).min(until.saturating_duration_since(Instant::now())));
    }
}

/// Starts timed.
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    if let Some(arg) = args.next()? {
        return Err(args.unknown(&arg));
    }
    for (signal, handler) in
        [(libc::SIGTERM, on_term as extern "C" fn(libc::c_int)), (libc::SIGHUP, on_hangup)]
    {
        // SAFETY: all zeros is a valid sigaction: numbers, a signal set
        // and a function address.
        let mut action: libc::sigaction = unsafe { util::zeroed() };
        action.sa_sigaction = handler as *const () as usize;
        // SAFETY: the handlers only store to an atomic.
        unsafe { libc::sigaction(signal, &raw const action, std::ptr::null_mut()) };
    }
    if clock::state(&SYS, SLOT_CLOCK).is_err() {
        return Err(util::fail(
            "timed: no clock capability: its service file needs admin.time".into(),
        ));
    }
    let mut timed = Timed {
        settings: Settings::default(),
        roots: Arc::new(RootCertStore::empty()),
        random: SystemRandom::new(),
        sessions: Vec::new(),
        synced: false,
        rtc_written: None,
        status: String::new(),
    };
    let _ = fs::write(PID, format!("{}\n", std::process::id()));
    timed.read_settings();
    Timed::not_before_last();
    let mut poll = POLL_MIN;
    while !STOP.load(Ordering::Relaxed) {
        if RELOAD.swap(false, Ordering::Relaxed) {
            println!("timed: reading {SETTINGS} again");
            timed.read_settings();
            poll = POLL_MIN;
        }
        let when = util::time_text(
            i64::try_from(now_ns() / 1_000_000_000).unwrap_or(0),
            "%F %T UTC",
            true,
        );
        let wait = match timed.round() {
            Ok((sample, from)) => match timed.apply(&sample) {
                Ok(did) => {
                    println!(
                        "timed: {from}: offset {:+.3} ms, delay {:.3} ms, stratum {}: {did}",
                        sample.offset as f64 / 1e6,
                        sample.delay as f64 / 1e6,
                        sample.stratum
                    );
                    let stepped = sample.offset.abs() > STEP_NS;
                    poll = if stepped { POLL_MIN } else { (poll * 2).min(POLL_MAX) };
                    timed.write_status(format!(
                        "Synchronized: yes, at {when}, from {from}\n\
                         Offset: {:+.3} ms, delay {:.3} ms, stratum {}: {did}\n\
                         Next round: in {} s\n",
                        sample.offset as f64 / 1e6,
                        sample.delay as f64 / 1e6,
                        sample.stratum,
                        poll.as_secs()
                    ));
                    poll
                }
                Err(why) => {
                    eprintln!("timed: {why}");
                    RETRY
                }
            },
            Err(why) => {
                let why = if why.is_empty() { vec!["no servers to ask".to_string()] } else { why };
                eprintln!("timed: no server gave the time: {}", why.join("; "));
                let synced = if timed.synced { "yes, earlier" } else { "no" };
                timed.write_status(format!(
                    "Synchronized: {synced}; at {when} no server gave the time:\n  {}\n\
                     Next round: in {} s\n",
                    why.join("\n  "),
                    RETRY.as_secs()
                ));
                RETRY
            }
        };
        sleep(wait);
    }
    let _ = fs::remove_file(PID);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_read() {
        let (settings, wrong) = parse_settings(
            "# Our own\nservers = time.example:4461 [2001:db8::1]:4460\nfallback =\n\
             certificates = /ca.pem\nspeed = fast\n",
        );
        assert_eq!(settings.servers, ["time.example:4461", "[2001:db8::1]:4460"]);
        assert!(settings.fallback.is_empty());
        assert_eq!(settings.certificates, ["/ca.pem"]);
        assert_eq!(wrong.len(), 1);
        assert_eq!(parse_settings("").0, Settings::default());
        assert_eq!(host_port("time.example:4461", 4460), ("time.example".into(), 4461));
        assert_eq!(host_port("[2001:db8::1]:4460", 1), ("2001:db8::1".into(), 4460));
        assert_eq!(host_port("2001:db8::1", 123), ("2001:db8::1".into(), 123));
        assert_eq!(host_port("pool.ntp.org", 123), ("pool.ntp.org".into(), 123));
    }

    #[test]
    fn samples_come_from_good_answers_only() {
        let t1 = 1_761_955_200_000_000_000i128;
        let request = ntp::request(77);
        let receive = ntp::timestamp(u64::try_from(t1 + 1_005_000_000).unwrap());
        let transmit = ntp::timestamp(u64::try_from(t1 + 1_006_000_000).unwrap());
        let answer = ntp::server_answer(&request, 2, receive, transmit, [0; 4]);
        // 5 ms each way, and 1 ms at the server.
        let sample = sample_of(&answer, t1, t1 + 11_000_000).unwrap();
        // A second ahead, give or take the timestamps' rounding.
        assert!((sample.offset - 1_000_000_000).abs() < 10, "{}", sample.offset);
        assert!((sample.delay - 10_000_000).abs() < 10, "{}", sample.delay);
        let kiss = ntp::server_answer(&request, 0, receive, transmit, *b"RATE");
        assert!(sample_of(&kiss, t1, t1).unwrap_err().contains("RATE"));
        let mut unsynced = answer.clone();
        unsynced[0] |= 0xc0;
        assert!(sample_of(&unsynced, t1, t1).is_err());
    }
}
