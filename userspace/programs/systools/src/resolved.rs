// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! resolved(8): the DNS resolver every program asks (ADR 0193).
//!
//! It answers on 127.0.0.53, port 53, over UDP and TCP, which the
//! resolv.conf it writes names, and `/etc/resolv.conf` leads to. It asks
//! the servers `/system/settings/resolved` (resolved(5)) names, or else
//! those DHCP gave, which netd(8) writes into its own resolv.conf, in
//! turn: a server that fails or does not answer within two seconds is
//! passed over for the next, and for half a minute after.
//!
//! A server named by address alone is asked over DNS over TLS on port
//! 853 first, taking any certificate (RFC 7858's opportunistic privacy
//! profile), and over plain DNS if it does not answer there, which is
//! tried again an hour later; `dns-over-tls = yes` has it asked over TLS
//! only, its certificate checked for its address, and `no` over plain
//! DNS. A server named with the name its certificate carries
//! (`9.9.9.9#dns.quad9.net`) is asked over TLS only, its certificate
//! checked, and one named by an `https://` URL over DNS over HTTPS (RFC
//! 8484). Plain queries go from a port the kernel picks at random each
//! time, with a random number, and an answer is taken only for the
//! question asked.
//!
//! Answers are kept for as long as their records' times to live say, a
//! day at most, and those that a name is not there as their SOA says
//! (RFC 2308), an hour at most; an answer given out of the cache has its
//! times to live counted down. `localhost` and the names below it are
//! answered here, and `.local` and `invalid` names are not there (RFC
//! 6761), so that they never leave the machine.
//!
//! The UDP queries come to the main thread, which answers those it can
//! from the cache and passes the rest to WORKERS threads, each with its
//! own connections to the servers it uses over TLS. A TCP connection has
//! a thread of its own. Another thread looks at the settings and netd's
//! resolv.conf every two seconds, and writes what resolvectl(1) shows.
//! SIGHUP reads the settings again and empties the cache; SIGUSR2 only
//! empties it; SIGTERM ends resolved.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{
    IpAddr, Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket,
};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use ring::rand::{SecureRandom, SystemRandom};
use rustls::ClientConfig;

use crate::args::Args;
use netproto::dns::{self, Question};
use crate::tls;
use crate::util::{self, Fail};

/// Where resolved answers.
pub const LISTEN: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 53), 53);
/// Its settings (resolved(5)).
pub const SETTINGS: &str = "/system/settings/resolved";
/// The resolv.conf netd(8) writes from what DHCP gives.
const NETD_RESOLV: &str = "/system/live/netd/resolv.conf";
/// The resolv.conf resolved writes, which `/etc/resolv.conf` leads to,
/// and what resolvectl(1) shows.
pub const STUB: &str = "/system/live/resolved/resolv.conf";
pub const STATUS: &str = "/system/live/resolved/status";
/// Where resolved keeps its process number, for resolvectl's
/// flush-caches.
pub const PID: &str = "/system/live/resolved/pid";

/// The threads that ask the servers.
const WORKERS: usize = 4;
/// The UDP queries that wait for a worker at most: past them, a query is
/// dropped, and its client asks again.
const BACKLOG: usize = 256;
/// The TCP connections served at once.
const TCP_MAX: usize = 16;
/// How long a TCP client may take to send a query.
const TCP_IDLE: Duration = Duration::from_secs(10);
/// How long a server has to answer, or to set up a connection.
const TIMEOUT: Duration = Duration::from_secs(2);
/// How long a server that failed is passed over.
const PASS_OVER: Duration = Duration::from_secs(30);
/// How long a server that does not answer DNS over TLS is asked over
/// plain DNS before TLS is tried again.
const TLS_RETRY: Duration = Duration::from_secs(3600);
/// How often the settings and netd's resolv.conf are looked at.
const WATCH: Duration = Duration::from_secs(2);
/// The answers kept, unless the settings say otherwise.
const CACHE_DEFAULT: usize = 4096;
/// The servers resolved asks at most.
const SERVERS_MAX: usize = 8;
/// The size of the largest answer it takes from a server over UDP, which
/// it offers in its queries' OPT records (the DNS flag day 2020's).
const UDP_SIZE: u16 = 1232;
/// The protocol DNS over TLS's connections offer (RFC 7858, ALPN "dot").
const DOT_ALPN: &[u8] = b"dot";
/// HTTP/1.1, for DNS over HTTPS.
const HTTP_ALPN: &[u8] = b"http/1.1";

static STOP: AtomicBool = AtomicBool::new(false);
static RELOAD: AtomicBool = AtomicBool::new(false);
static FLUSH: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

extern "C" fn on_hangup(_: libc::c_int) {
    RELOAD.store(true, Ordering::Relaxed);
}

extern "C" fn on_usr2(_: libc::c_int) {
    FLUSH.store(true, Ordering::Relaxed);
}

/// How servers named by address alone are asked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverTls {
    /// Over TLS if they answer there, taking any certificate, else plain.
    #[default]
    Opportunistic,
    /// Over TLS only, their certificates checked for their addresses.
    Yes,
    /// Over plain DNS.
    No,
}

/// How a server is asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// By address alone: as [`OverTls`] says.
    Address,
    /// Over TLS, its certificate checked for this name.
    Tls(String),
    /// Over HTTPS at this host and path.
    Https { host: String, path: String },
}

/// A server resolved asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    /// As the settings or netd wrote it.
    pub text: String,
    /// Its address, and the port of what it is asked over: plain DNS's
    /// for one named by address alone (DNS over TLS goes to 853).
    pub address: SocketAddr,
    pub kind: Kind,
    /// Whether DHCP gave it.
    pub from_dhcp: bool,
}

/// What resolved(5) says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    pub servers: Vec<Server>,
    pub over_tls: OverTls,
    pub cache_size: usize,
    /// More authorities' certificates to trust, PEM files.
    pub certificates: Vec<String>,
}

/// Reads a server as resolved(5) writes one: `address[:port][#name]`,
/// with an IPv6 address in brackets when it has a port, or
/// `https://host[:port]/path#address`, whose address may be left out
/// when the host is one.
pub fn parse_server(text: &str) -> Result<Server, String> {
    if let Some(rest) = text.strip_prefix("https://") {
        let (url, address) = match rest.split_once('#') {
            Some((url, address)) => (url, Some(address)),
            None => (rest, None),
        };
        let (authority, path) = match url.find('/') {
            Some(at) => (&url[..at], &url[at..]),
            None => (url, "/dns-query"),
        };
        let (host, port) = split_port(authority, 443)?;
        let ip = match address {
            Some(address) => address
                .parse::<IpAddr>()
                .map_err(|_| format!("'{address}' is not an address, in '{text}'"))?,
            None => host.parse::<IpAddr>().map_err(|_| {
                format!("'{text}' needs the address to reach {host} at, after a '#'")
            })?,
        };
        if host.is_empty() || path.contains(char::is_whitespace) {
            return Err(format!("'{text}' is not a URL"));
        }
        return Ok(Server {
            text: text.to_string(),
            address: SocketAddr::new(ip, port),
            kind: Kind::Https { host: host.to_string(), path: path.to_string() },
            from_dhcp: false,
        });
    }
    let (address, name) = match text.split_once('#') {
        Some((address, name)) if !name.is_empty() => (address, Some(name)),
        Some(_) => return Err(format!("'{text}' has no name after its '#'")),
        None => (text, None),
    };
    let default_port = if name.is_some() { 853 } else { 53 };
    let (host, port) = split_port(address, default_port)?;
    let ip = host.parse::<IpAddr>().map_err(|_| format!("'{host}' is not an address"))?;
    let kind = match name {
        Some(name) => {
            rustls::pki_types::ServerName::try_from(name)
                .map_err(|_| format!("'{name}' is not a server's name"))?;
            Kind::Tls(name.to_string())
        }
        None => Kind::Address,
    };
    Ok(Server { text: text.to_string(), address: SocketAddr::new(ip, port), kind, from_dhcp: false })
}

/// Splits `host[:port]`, or `[v6]:port`, taking `default` for a port
/// left out.
fn split_port(text: &str, default: u16) -> Result<(&str, u16), String> {
    let number = |port: &str| port.parse::<u16>().map_err(|_| format!("'{port}' is not a port"));
    if let Some(rest) = text.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or_else(|| format!("'{text}' has no ']'"))?;
        return match after.strip_prefix(':') {
            Some(port) => Ok((host, number(port)?)),
            None if after.is_empty() => Ok((host, default)),
            None => Err(format!("'{text}' is not an address")),
        };
    }
    match text.rsplit_once(':') {
        // An IPv6 address without brackets has no port.
        Some((host, _)) if host.contains(':') => Ok((text, default)),
        Some((host, port)) => Ok((host, number(port)?)),
        None => Ok((text, default)),
    }
}

/// Reads resolved(5)'s `text`: the settings, and what is wrong with any
/// line, which is left out.
pub fn parse_settings(text: &str) -> (Settings, Vec<String>) {
    let mut settings = Settings { cache_size: CACHE_DEFAULT, ..Settings::default() };
    let mut wrong = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = comment_free(line);
        if line.trim().is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            wrong.push(format!("{SETTINGS}:{}: no '=' in '{}'", number + 1, line.trim()));
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        let mut bad = |why: String| wrong.push(format!("{SETTINGS}:{}: {why}", number + 1));
        match key {
            "servers" => {
                for word in value.split_whitespace() {
                    match parse_server(word) {
                        Ok(server) if settings.servers.len() < SERVERS_MAX => {
                            settings.servers.push(server);
                        }
                        Ok(_) => bad(format!("more than {SERVERS_MAX} servers")),
                        Err(why) => bad(why),
                    }
                }
            }
            "dns-over-tls" => match value {
                "opportunistic" => settings.over_tls = OverTls::Opportunistic,
                "yes" => settings.over_tls = OverTls::Yes,
                "no" => settings.over_tls = OverTls::No,
                _ => bad(format!("dns-over-tls is opportunistic, yes or no, not '{value}'")),
            },
            "cache-size" => match value.parse() {
                Ok(size) => settings.cache_size = size,
                Err(_) => bad(format!("cache-size is a number of answers, not '{value}'")),
            },
            "certificates" => {
                settings.certificates.extend(value.split_whitespace().map(String::from));
            }
            _ => bad(format!("no setting '{key}'")),
        }
    }
    (settings, wrong)
}

/// `line` without its comment: from a '#' at the start of the line or
/// after white space, as servers have '#' in them too.
fn comment_free(line: &str) -> &str {
    let mut previous = ' ';
    for (at, c) in line.char_indices() {
        if c == '#' && previous.is_whitespace() {
            return &line[..at];
        }
        previous = c;
    }
    line
}

/// The servers and search domains of a resolv.conf.
pub fn parse_resolv(text: &str) -> (Vec<Server>, Vec<String>) {
    let mut servers = Vec::new();
    let mut search = Vec::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        match words.next() {
            Some("nameserver") => {
                if let Some(Ok(ip)) = words.next().map(str::parse::<IpAddr>)
                    && servers.len() < SERVERS_MAX
                    && ip != IpAddr::V4(*LISTEN.ip())
                {
                    servers.push(Server {
                        text: ip.to_string(),
                        address: SocketAddr::new(ip, 53),
                        kind: Kind::Address,
                        from_dhcp: true,
                    });
                }
            }
            Some("search" | "domain") => search = words.map(String::from).collect(),
            _ => {}
        }
    }
    (servers, search)
}

/// The resolv.conf resolved writes: itself as the one server, and the
/// search domains DHCP gave.
pub fn stub_text(search: &[String]) -> String {
    let mut text = String::from(
        "# Written by resolved(8): programs ask it, and it asks the servers\n\
         # resolvectl status shows. /etc/resolv.conf leads here.\n\
         nameserver 127.0.0.53\n\
         options edns0 trust-ad\n",
    );
    if !search.is_empty() {
        text += &format!("search {}\n", search.join(" "));
    }
    text
}

/// What a server's health is.
#[derive(Clone, Copy, Debug, Default)]
struct Health {
    /// Until when it is passed over, after failing.
    failed_until: Option<Instant>,
    /// Until when it is asked over plain DNS, after not answering over TLS.
    plain_until: Option<Instant>,
    /// How it answered last.
    last: Option<Transport>,
    answered: u64,
    failed: u64,
}

/// How a query went to a server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    Udp,
    Tcp,
    /// DNS over TLS, whether the certificate was checked.
    Tls { checked: bool },
    Https,
}

impl Transport {
    fn text(self) -> &'static str {
        match self {
            Transport::Udp => "plain DNS over UDP",
            Transport::Tcp => "plain DNS over TCP",
            Transport::Tls { checked: true } => "DNS over TLS, its certificate checked",
            Transport::Tls { checked: false } => "DNS over TLS, any certificate (opportunistic)",
            Transport::Https => "DNS over HTTPS",
        }
    }
}

/// An answer kept.
struct Entry {
    answer: Vec<u8>,
    stored: Instant,
    expires: Instant,
}

/// What an answer is kept under: its question, whether DNSSEC records
/// were asked for, and whether checking was disabled.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    name: String,
    qtype: u16,
    qclass: u16,
    dnssec: bool,
    unchecked: bool,
}

impl Key {
    fn of(query: &[u8], question: &Question) -> Key {
        Key {
            name: question.name.clone(),
            qtype: question.qtype,
            qclass: question.qclass,
            dnssec: dns::wants_dnssec(query),
            unchecked: dns::header(query).is_some_and(|h| h.flags & dns::CD != 0),
        }
    }
}

/// The answers kept.
#[derive(Default)]
struct Cache {
    entries: HashMap<Key, Entry>,
    size: usize,
}

impl Cache {
    fn get(&mut self, key: &Key, now: Instant) -> Option<Vec<u8>> {
        let entry = self.entries.get(key)?;
        if entry.expires <= now {
            self.entries.remove(key);
            return None;
        }
        let mut answer = entry.answer.clone();
        let age = now.duration_since(entry.stored).as_secs();
        dns::age(&mut answer, u32::try_from(age).unwrap_or(u32::MAX));
        Some(answer)
    }

    fn put(&mut self, key: Key, answer: &[u8], ttl: u32, now: Instant) {
        if self.size == 0 || ttl == 0 {
            return;
        }
        if self.entries.len() >= self.size && !self.entries.contains_key(&key) {
            self.entries.retain(|_, entry| entry.expires > now);
            if self.entries.len() >= self.size {
                // The one closest to going.
                if let Some(first) =
                    self.entries.iter().min_by_key(|(_, entry)| entry.expires).map(|(k, _)| k.clone())
                {
                    self.entries.remove(&first);
                }
            }
        }
        let expires = now + Duration::from_secs(u64::from(ttl));
        self.entries.insert(key, Entry { answer: answer.to_vec(), stored: now, expires });
    }
}

/// What resolved shares between its threads.
struct Shared {
    settings: RwLock<Arc<Settings>>,
    /// The servers asked, from the settings or else netd's resolv.conf,
    /// and their health, in the same order.
    servers: RwLock<Arc<Vec<Server>>>,
    health: Mutex<Vec<Health>>,
    /// Bumped when the servers change, so that the workers drop their
    /// connections.
    generation: AtomicU64,
    cache: Mutex<Cache>,
    verified: RwLock<Arc<ClientConfig>>,
    verified_https: RwLock<Arc<ClientConfig>>,
    opportunistic: Arc<ClientConfig>,
    random: SystemRandom,
    queries: AtomicU64,
    from_cache: AtomicU64,
    local: AtomicU64,
    failures: AtomicU64,
    tcp_clients: AtomicUsize,
}

fn read<T: Clone>(lock: &RwLock<T>) -> T {
    lock.read().unwrap_or_else(PoisonError::into_inner).clone()
}

fn write<T>(lock: &RwLock<T>, value: T) {
    *lock.write().unwrap_or_else(PoisonError::into_inner) = value;
}

impl Shared {
    fn random_u16(&self) -> u16 {
        let mut bytes = [0; 2];
        let _ = self.random.fill(&mut bytes);
        u16::from_ne_bytes(bytes)
    }

    /// Answers `query`, through `links`: from here, the cache or a server.
    /// `None` for one that is not a query to answer.
    fn answer(&self, links: &mut Links, query: &[u8]) -> Option<Vec<u8>> {
        let header = dns::header(query)?;
        if header.flags & dns::QR != 0 {
            return None;
        }
        self.queries.fetch_add(1, Ordering::Relaxed);
        let Some(question) = dns::question(query) else {
            // FORMERR, with the header alone.
            let mut out = query.get(..dns::HEADER)?.to_vec();
            out[2..4].copy_from_slice(&(dns::QR | dns::RA | 1).to_be_bytes());
            out[4..12].fill(0);
            return Some(out);
        };
        if let Some(answer) = dns::local_answer(query, &question) {
            self.local.fetch_add(1, Ordering::Relaxed);
            return Some(answer);
        }
        let key = Key::of(query, &question);
        if let Some(answer) = self.cached(&key, header.id) {
            return Some(answer);
        }
        match self.ask(links, query, &question) {
            Some(mut answer) => {
                if let Some(ttl) = dns::keep_for(&answer) {
                    let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
                    cache.put(key, &answer, ttl, Instant::now());
                }
                dns::set_id(&mut answer, header.id);
                Some(answer)
            }
            None => {
                self.failures.fetch_add(1, Ordering::Relaxed);
                Some(dns::failure(query, &question, dns::SERVFAIL))
            }
        }
    }

    /// The answer kept for `key`, numbered `id`.
    fn cached(&self, key: &Key, id: u16) -> Option<Vec<u8>> {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        let mut answer = cache.get(key, Instant::now())?;
        drop(cache);
        self.from_cache.fetch_add(1, Ordering::Relaxed);
        dns::set_id(&mut answer, id);
        Some(answer)
    }

    /// Asks the servers in turn for `query`'s answer: the first that
    /// answers, or failing that, the last failure one gave.
    fn ask(&self, links: &mut Links, query: &[u8], question: &Question) -> Option<Vec<u8>> {
        let servers = read(&self.servers);
        let generation = self.generation.load(Ordering::Acquire);
        links.renew(generation, servers.len());
        let now = Instant::now();
        let health = self.health.lock().unwrap_or_else(PoisonError::into_inner).clone();
        // Those not passed over first, then those that are, in order.
        let resting =
            |i: usize| health.get(i).and_then(|h| h.failed_until).is_some_and(|t| t > now);
        let mut order: Vec<usize> = (0..servers.len()).filter(|&i| !resting(i)).collect();
        order.extend((0..servers.len()).filter(|&i| resting(i)));
        let settings = read(&self.settings);
        let mut fallback = None;
        for index in order {
            let server = &servers[index];
            let plain = health.get(index).and_then(|h| h.plain_until).is_some_and(|t| t > now);
            let result = self.ask_one(links, index, server, settings.over_tls, plain, query, question);
            let mut health = self.health.lock().unwrap_or_else(PoisonError::into_inner);
            if self.generation.load(Ordering::Acquire) != generation {
                return result.ok().map(|(answer, _)| answer);
            }
            let Some(h) = health.get_mut(index) else { break };
            match result {
                Ok((answer, how)) => {
                    let rcode = dns::header(&answer).map_or(dns::SERVFAIL, |h| h.rcode());
                    h.last = Some(how);
                    if matches!(how, Transport::Udp | Transport::Tcp)
                        && settings.over_tls == OverTls::Opportunistic
                        && server.kind == Kind::Address
                        && !plain
                    {
                        h.plain_until = Some(now + TLS_RETRY);
                    }
                    if rcode == dns::SERVFAIL || rcode == dns::REFUSED {
                        h.failed += 1;
                        fallback = Some(answer);
                        continue;
                    }
                    h.answered += 1;
                    h.failed_until = None;
                    return Some(answer);
                }
                Err(_) => {
                    h.failed += 1;
                    h.failed_until = Some(now + PASS_OVER);
                }
            }
        }
        fallback
    }

    /// Asks `server`, the `index`th, for `query`'s answer, as it is asked:
    /// the answer, and how it came.
    #[allow(clippy::too_many_arguments)]
    fn ask_one(
        &self,
        links: &mut Links,
        index: usize,
        server: &Server,
        over_tls: OverTls,
        plain: bool,
        query: &[u8],
        question: &Question,
    ) -> io::Result<(Vec<u8>, Transport)> {
        match &server.kind {
            Kind::Https { host, path } => {
                let config = read(&self.verified_https);
                let answer = links.https(index, server.address, host, path, &config, query)?;
                checked(answer, question).map(|answer| (answer, Transport::Https))
            }
            Kind::Tls(name) => {
                let config = read(&self.verified);
                let address = server.address;
                let answer = links.tls(index, address, Some(name), &config, self, query, question)?;
                Ok((answer, Transport::Tls { checked: true }))
            }
            Kind::Address => {
                let tls_address = SocketAddr::new(server.address.ip(), 853);
                match over_tls {
                    OverTls::Yes => {
                        let config = read(&self.verified);
                        let answer =
                            links.tls(index, tls_address, None, &config, self, query, question)?;
                        Ok((answer, Transport::Tls { checked: true }))
                    }
                    OverTls::Opportunistic if !plain => {
                        let config = self.opportunistic.clone();
                        match links.tls(index, tls_address, None, &config, self, query, question) {
                            Ok(answer) => Ok((answer, Transport::Tls { checked: false })),
                            Err(_) => self.plain(server.address, query, question),
                        }
                    }
                    _ => self.plain(server.address, query, question),
                }
            }
        }
    }

    /// Asks `address` over plain DNS: UDP, then TCP if the answer was cut.
    fn plain(
        &self,
        address: SocketAddr,
        query: &[u8],
        question: &Question,
    ) -> io::Result<(Vec<u8>, Transport)> {
        let mut message = query.to_vec();
        let id = self.random_u16();
        dns::set_id(&mut message, id);
        dns::limit_udp_size(&mut message, UDP_SIZE);
        let socket = UdpSocket::bind(SocketAddr::new(unspecified(address), 0))?;
        socket.connect(address)?;
        socket.send(&message)?;
        let deadline = Instant::now() + TIMEOUT;
        let mut buf = vec![0; 65_535];
        let answer = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            socket.set_read_timeout(Some(left))?;
            let len = socket.recv(&mut buf)?;
            let got = &buf[..len];
            // Anything else is not the answer: a stray or forged datagram.
            if dns::header(got).is_some_and(|h| h.id == id) && dns::answers(got, question) {
                break got.to_vec();
            }
        };
        if dns::header(&answer).is_some_and(|h| h.flags & dns::TC != 0) {
            let mut stream = TcpStream::connect_timeout(&address, TIMEOUT)?;
            stream.set_read_timeout(Some(TIMEOUT))?;
            stream.set_write_timeout(Some(TIMEOUT))?;
            let answer = exchange(&mut stream, &message)?;
            let answer = checked(answer, question)?;
            if dns::header(&answer).is_some_and(|h| h.id == id) {
                return Ok((answer, Transport::Tcp));
            }
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok((answer, Transport::Udp))
    }
}

/// The unspecified address of `address`'s family, to bind to.
fn unspecified(address: SocketAddr) -> IpAddr {
    match address {
        SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        SocketAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
    }
}

/// `answer` if it answers `question`.
fn checked(answer: Vec<u8>, question: &Question) -> io::Result<Vec<u8>> {
    if dns::answers(&answer, question) {
        Ok(answer)
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidData, "not the answer to the question asked"))
    }
}

/// Sends `message` over a stream with its length in front (RFC 1035's
/// TCP framing), and reads the answer framed so.
fn exchange<S: Read + Write>(stream: &mut S, message: &[u8]) -> io::Result<Vec<u8>> {
    let len = u16::try_from(message.len()).map_err(|_| io::ErrorKind::InvalidInput)?;
    let mut framed = Vec::with_capacity(message.len() + 2);
    framed.extend_from_slice(&len.to_be_bytes());
    framed.extend_from_slice(message);
    stream.write_all(&framed)?;
    stream.flush()?;
    let mut len = [0; 2];
    stream.read_exact(&mut len)?;
    let mut answer = vec![0; usize::from(u16::from_be_bytes(len))];
    stream.read_exact(&mut answer)?;
    Ok(answer)
}

/// A worker's connections to the servers it asks over TLS or HTTPS, by
/// server, kept between queries.
#[derive(Default)]
struct Links {
    generation: u64,
    streams: Vec<Option<tls::Stream>>,
}

impl Links {
    /// Drops every connection if the servers changed since they were made.
    fn renew(&mut self, generation: u64, servers: usize) {
        if self.generation != generation || self.streams.len() != servers {
            self.generation = generation;
            self.streams = (0..servers).map(|_| None).collect();
        }
    }

    /// The connection to server `index`, at `address`, made with `config`
    /// for `name` (or the address) if there is none; `fresh` if it was
    /// just made.
    fn stream(
        &mut self,
        index: usize,
        address: SocketAddr,
        name: Option<&str>,
        config: &Arc<ClientConfig>,
    ) -> io::Result<(&mut tls::Stream, bool)> {
        let slot = self.streams.get_mut(index).ok_or(io::ErrorKind::NotFound)?;
        let fresh = slot.is_none();
        if fresh {
            let server_name = tls::server_name(name, address)?;
            *slot = Some(tls::connect(address, server_name, config, TIMEOUT)?);
        }
        match slot {
            Some(stream) => Ok((stream, fresh)),
            None => Err(io::ErrorKind::NotConnected.into()),
        }
    }

    /// Asks server `index` over DNS over TLS, on a connection kept or made,
    /// once more on a new one if a kept one has closed.
    #[allow(clippy::too_many_arguments)]
    fn tls(
        &mut self,
        index: usize,
        address: SocketAddr,
        name: Option<&str>,
        config: &Arc<ClientConfig>,
        shared: &Shared,
        query: &[u8],
        question: &Question,
    ) -> io::Result<Vec<u8>> {
        let mut message = query.to_vec();
        let id = shared.random_u16();
        dns::set_id(&mut message, id);
        loop {
            let (stream, fresh) = self.stream(index, address, name, config)?;
            match exchange(stream, &message).and_then(|answer| checked(answer, question)) {
                Ok(answer) if dns::header(&answer).is_some_and(|h| h.id == id) => {
                    return Ok(answer);
                }
                result => {
                    self.streams[index] = None;
                    if fresh {
                        return result.and(Err(io::ErrorKind::InvalidData.into()));
                    }
                }
            }
        }
    }

    /// Asks server `index` over DNS over HTTPS, POSTing the query (RFC
    /// 8484) with the number 0 it asks for, on a connection kept or made.
    fn https(
        &mut self,
        index: usize,
        address: SocketAddr,
        host: &str,
        path: &str,
        config: &Arc<ClientConfig>,
        query: &[u8],
    ) -> io::Result<Vec<u8>> {
        let mut message = query.to_vec();
        dns::set_id(&mut message, 0);
        let mut request = format!(
            "POST {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: resolved (Dukovo)\r\n\
             Content-Type: application/dns-message\r\nAccept: application/dns-message\r\n\
             Content-Length: {}\r\n\r\n",
            message.len()
        )
        .into_bytes();
        request.extend_from_slice(&message);
        loop {
            let (stream, fresh) = self.stream(index, address, Some(host), config)?;
            let result = tls::send(stream, &request).and_then(|()| http_response(stream));
            match result {
                Ok((body, keep)) => {
                    if !keep {
                        self.streams[index] = None;
                    }
                    return Ok(body);
                }
                Err(err) => {
                    self.streams[index] = None;
                    if fresh || err.kind() == io::ErrorKind::InvalidData {
                        return Err(err);
                    }
                }
            }
        }
    }
}

/// Reads an HTTP/1.1 response to a DNS over HTTPS query: its body, if
/// it is a 200 with a DNS message, and whether the connection stays open.
/// The body comes with a length or in chunks.
fn http_response<R: Read>(stream: R) -> io::Result<(Vec<u8>, bool)> {
    let bad = |why: &str| io::Error::new(io::ErrorKind::InvalidData, why.to_string());
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.is_empty() {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let status = line.split_whitespace().nth(1).unwrap_or_default().to_string();
    let mut length = None;
    let mut chunked = false;
    let mut keep = !line.starts_with("HTTP/1.0");
    let mut dns_message = false;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else { continue };
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.parse::<usize>().ok(),
            "transfer-encoding" => chunked = value.eq_ignore_ascii_case("chunked"),
            "connection" => keep = !value.eq_ignore_ascii_case("close"),
            "content-type" => dns_message = value.starts_with("application/dns-message"),
            _ => {}
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            line.clear();
            reader.read_line(&mut line)?;
            let size = line.trim().split(';').next().unwrap_or_default();
            let size = usize::from_str_radix(size, 16).map_err(|_| bad("a bad chunk"))?;
            if size == 0 {
                // The trailer, to the empty line.
                loop {
                    line.clear();
                    if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                break;
            }
            if body.len() + size > 65_535 {
                return Err(bad("too long an answer"));
            }
            let start = body.len();
            body.resize(start + size, 0);
            reader.read_exact(&mut body[start..])?;
            line.clear();
            reader.read_line(&mut line)?;
        }
    } else {
        let length = length.ok_or_else(|| bad("an answer with no length"))?;
        if length > 65_535 {
            return Err(bad("too long an answer"));
        }
        body.resize(length, 0);
        reader.read_exact(&mut body)?;
    }
    if status != "200" || !dns_message {
        return Err(bad(&format!("HTTP status {status}")));
    }
    // Nothing is read past the body, so nothing buffered is lost.
    Ok((body, keep))
}

/// Starts resolved.
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    if let Some(arg) = args.next()? {
        return Err(args.unknown(&arg));
    }
    for (signal, handler) in [
        (libc::SIGTERM, on_term as extern "C" fn(libc::c_int)),
        (libc::SIGHUP, on_hangup),
        (libc::SIGUSR2, on_usr2),
    ] {
        // SAFETY: all zeros is a valid sigaction: numbers, a signal set
        // and a function address.
        let mut action: libc::sigaction = unsafe { util::zeroed() };
        action.sa_sigaction = handler as *const () as usize;
        // SAFETY: the handlers only store to an atomic.
        unsafe { libc::sigaction(signal, &raw const action, std::ptr::null_mut()) };
    }
    let udp = UdpSocket::bind(LISTEN).map_err(|e| util::failed(&"resolved: port 53", e))?;
    let tcp = TcpListener::bind(LISTEN).map_err(|e| util::failed(&"resolved: port 53", e))?;
    let shared = Arc::new(Shared {
        settings: RwLock::new(Arc::new(Settings::default())),
        servers: RwLock::new(Arc::new(Vec::new())),
        health: Mutex::new(Vec::new()),
        generation: AtomicU64::new(0),
        cache: Mutex::new(Cache::default()),
        verified: RwLock::new(tls::verified(rustls::RootCertStore::empty(), &[DOT_ALPN])),
        verified_https: RwLock::new(tls::verified(rustls::RootCertStore::empty(), &[HTTP_ALPN])),
        opportunistic: tls::opportunistic(&[DOT_ALPN]),
        random: SystemRandom::new(),
        queries: AtomicU64::new(0),
        from_cache: AtomicU64::new(0),
        local: AtomicU64::new(0),
        failures: AtomicU64::new(0),
        tcp_clients: AtomicUsize::new(0),
    });
    let mut watcher = Watcher::default();
    watcher.look(&shared, true);
    let _ = fs::write(PID, format!("{}\n", std::process::id()));
    println!("resolved: answering on {LISTEN} over UDP and TCP");
    let (queue, jobs) = mpsc::sync_channel::<(Vec<u8>, SocketAddr)>(BACKLOG);
    let jobs = Arc::new(Mutex::new(jobs));
    for n in 0..WORKERS {
        let (shared, jobs) = (shared.clone(), jobs.clone());
        let reply = udp.try_clone().map_err(|e| util::failed(&"resolved", e))?;
        thread::Builder::new()
            .name(format!("worker{n}"))
            .spawn(move || work(&shared, &jobs, &reply))
            .map_err(|e| util::failed(&"resolved: a worker", e))?;
    }
    {
        let shared = shared.clone();
        thread::Builder::new()
            .name("tcp".into())
            .spawn(move || serve_tcp(&shared, &tcp))
            .map_err(|e| util::failed(&"resolved: the TCP thread", e))?;
    }
    {
        let shared = shared.clone();
        thread::Builder::new()
            .name("watch".into())
            .spawn(move || {
                loop {
                    thread::sleep(WATCH);
                    watcher.look(&shared, false);
                }
            })
            .map_err(|e| util::failed(&"resolved: the watching thread", e))?;
    }
    serve_udp(&shared, &udp, &queue);
    let _ = fs::remove_file(PID);
    Ok(0)
}

/// Takes the UDP queries: answers those it can from here or the cache,
/// and passes the others on.
fn serve_udp(shared: &Shared, udp: &UdpSocket, queue: &SyncSender<(Vec<u8>, SocketAddr)>) {
    let mut buf = vec![0; 65_535];
    let mut nothing = Links::default();
    while !STOP.load(Ordering::Relaxed) {
        let Ok((len, from)) = udp.recv_from(&mut buf) else { continue };
        let query = &buf[..len];
        let Some(question) = dns::question(query) else {
            if let Some(answer) = shared.answer(&mut nothing, query) {
                let _ = udp.send_to(&answer, from);
            }
            continue;
        };
        let header = dns::header(query).map_or(0, |h| h.id);
        let local = dns::local_answer(query, &question).inspect(|_| {
            shared.queries.fetch_add(1, Ordering::Relaxed);
            shared.local.fetch_add(1, Ordering::Relaxed);
        });
        let quick = local.or_else(|| {
            let key = Key::of(query, &question);
            let answer = shared.cached(&key, header);
            if answer.is_some() {
                shared.queries.fetch_add(1, Ordering::Relaxed);
            }
            answer
        });
        if let Some(answer) = quick {
            let _ = udp.send_to(&dns::fit(answer, dns::udp_size(query)), from);
            continue;
        }
        match queue.try_send((query.to_vec(), from)) {
            Ok(()) | Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => return,
        }
    }
}

/// A worker: answers the queries passed to it, over `reply`.
fn work(shared: &Shared, jobs: &Mutex<Receiver<(Vec<u8>, SocketAddr)>>, reply: &UdpSocket) {
    let mut links = Links::default();
    loop {
        let job = jobs.lock().unwrap_or_else(PoisonError::into_inner).recv();
        let Ok((query, from)) = job else { return };
        if let Some(answer) = shared.answer(&mut links, &query) {
            let _ = reply.send_to(&dns::fit(answer, dns::udp_size(&query)), from);
        }
    }
}

/// Takes TCP connections, each in a thread of its own.
fn serve_tcp(shared: &Arc<Shared>, tcp: &TcpListener) {
    for stream in tcp.incoming() {
        let Ok(stream) = stream else { continue };
        if shared.tcp_clients.fetch_add(1, Ordering::AcqRel) >= TCP_MAX {
            shared.tcp_clients.fetch_sub(1, Ordering::AcqRel);
            continue;
        }
        let shared = shared.clone();
        let started = thread::Builder::new().name("client".into()).spawn(move || {
            serve_client(&shared, stream);
            shared.tcp_clients.fetch_sub(1, Ordering::AcqRel);
        });
        if started.is_err() {
            // The closure, and its count, went with the failure.
            eprintln!("resolved: cannot start a thread for a TCP client");
        }
    }
}

/// Answers a TCP client's queries until it stops sending them.
fn serve_client(shared: &Shared, mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(TCP_IDLE));
    let mut links = Links::default();
    loop {
        let mut len = [0; 2];
        if stream.read_exact(&mut len).is_err() {
            break;
        }
        let mut query = vec![0; usize::from(u16::from_be_bytes(len))];
        if stream.read_exact(&mut query).is_err() {
            break;
        }
        let Some(answer) = shared.answer(&mut links, &query) else { break };
        let Ok(len) = u16::try_from(answer.len()) else { break };
        let mut framed = len.to_be_bytes().to_vec();
        framed.extend_from_slice(&answer);
        if stream.write_all(&framed).is_err() {
            break;
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
}

/// What the watching thread saw last.
#[derive(Default)]
struct Watcher {
    settings_time: Option<SystemTime>,
    netd_time: Option<SystemTime>,
    stub: String,
    status: String,
}

fn modified(path: &str) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl Watcher {
    /// Reads the settings and netd's resolv.conf again if they changed, or
    /// were asked to be read; empties the cache when asked; and writes the
    /// stub resolv.conf and the status if they changed.
    fn look(&mut self, shared: &Shared, first: bool) {
        let reload = RELOAD.swap(false, Ordering::Relaxed);
        if reload {
            println!("resolved: reading {SETTINGS} again");
        }
        let settings_time = modified(SETTINGS);
        let netd_time = modified(NETD_RESOLV);
        let changed = first
            || reload
            || settings_time != self.settings_time
            || netd_time != self.netd_time;
        if changed {
            self.settings_time = settings_time;
            self.netd_time = netd_time;
            self.read(shared, first || reload);
        }
        if FLUSH.swap(false, Ordering::Relaxed) || reload {
            shared.cache.lock().unwrap_or_else(PoisonError::into_inner).entries.clear();
            println!("resolved: emptied the cache");
        }
        let status = status_text(shared);
        if status != self.status && write_file(STATUS, &status) {
            self.status = status;
        }
    }

    /// Reads the settings and netd's resolv.conf.
    fn read(&mut self, shared: &Shared, say: bool) {
        let (settings, wrong) = match fs::read_to_string(SETTINGS) {
            Ok(text) => parse_settings(&text),
            Err(_) => (Settings { cache_size: CACHE_DEFAULT, ..Settings::default() }, Vec::new()),
        };
        for line in &wrong {
            eprintln!("resolved: {line}");
        }
        let (dhcp, search) =
            fs::read_to_string(NETD_RESOLV).map_or_default(|text| parse_resolv(&text));
        let servers = if settings.servers.is_empty() { dhcp } else { settings.servers.clone() };
        let old = read(&shared.servers);
        let old_settings = read(&shared.settings);
        if *old != servers || old_settings.certificates != settings.certificates || say {
            let roots = tls::roots(&settings.certificates, "resolved");
            write(&shared.verified, tls::verified(roots.clone(), &[DOT_ALPN]));
            write(&shared.verified_https, tls::verified(roots, &[HTTP_ALPN]));
        }
        if *old != servers || say {
            let names: Vec<&str> = servers.iter().map(|s| s.text.as_str()).collect();
            let from = if settings.servers.is_empty() { "from DHCP" } else { "from its settings" };
            if names.is_empty() {
                println!("resolved: no DNS servers to ask yet");
            } else {
                println!("resolved: asking {} ({from})", names.join(" "));
            }
            *shared.health.lock().unwrap_or_else(PoisonError::into_inner) =
                vec![Health::default(); servers.len()];
            write(&shared.servers, Arc::new(servers));
            shared.generation.fetch_add(1, Ordering::AcqRel);
        }
        shared.cache.lock().unwrap_or_else(PoisonError::into_inner).size = settings.cache_size;
        write(&shared.settings, Arc::new(settings));
        let stub = stub_text(&search);
        if stub != self.stub && write_file(STUB, &stub) {
            self.stub = stub;
        }
    }
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
        eprintln!("resolved: cannot write {path}: {}", util::strip_os_error(err));
    }
    written.is_ok()
}

/// What resolvectl status shows.
fn status_text(shared: &Shared) -> String {
    let settings = read(&shared.settings);
    let servers = read(&shared.servers);
    let health = shared.health.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let cache = shared.cache.lock().unwrap_or_else(PoisonError::into_inner);
    let (kept, size) = (cache.entries.len(), cache.size);
    drop(cache);
    let over_tls = match settings.over_tls {
        OverTls::Opportunistic => "opportunistic",
        OverTls::Yes => "yes",
        OverTls::No => "no",
    };
    let mut text = format!(
        "Listening on: {LISTEN} (UDP and TCP)\n\
         DNS over TLS: {over_tls}\n\
         Cache: {kept} of {size} answers\n\
         Queries: {}, {} from the cache, {} answered here, {} failed\n",
        shared.queries.load(Ordering::Relaxed),
        shared.from_cache.load(Ordering::Relaxed),
        shared.local.load(Ordering::Relaxed),
        shared.failures.load(Ordering::Relaxed),
    );
    if servers.is_empty() {
        text += "Servers: none: no settings, and DHCP gave none\n";
    }
    for (server, health) in servers.iter().zip(&health) {
        let from = if server.from_dhcp { "from DHCP" } else { "from the settings" };
        let how = match (&server.kind, health.last) {
            (_, Some(how)) => how.text().to_string(),
            (Kind::Https { .. }, None) => "DNS over HTTPS, not asked yet".into(),
            (Kind::Tls(name), None) => format!("DNS over TLS for {name}, not asked yet"),
            (Kind::Address, None) => "not asked yet".into(),
        };
        text += &format!(
            "Server: {} ({from}): {how}; {} answered, {} failed\n",
            server.text, health.answered, health.failed
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_read_as_resolved_5_writes_them() {
        let plain = parse_server("192.168.1.1").unwrap();
        assert_eq!((plain.address, &plain.kind), ("192.168.1.1:53".parse().unwrap(), &Kind::Address));
        let tls = parse_server("9.9.9.9#dns.quad9.net").unwrap();
        assert_eq!(tls.address, "9.9.9.9:853".parse().unwrap());
        assert_eq!(tls.kind, Kind::Tls("dns.quad9.net".into()));
        let test = parse_server("127.0.0.1:5353").unwrap();
        assert_eq!(test.address, "127.0.0.1:5353".parse().unwrap());
        let v6 = parse_server("[2620:fe::fe]:853#dns.quad9.net").unwrap();
        assert_eq!(v6.address, "[2620:fe::fe]:853".parse().unwrap());
        let v6 = parse_server("2620:fe::fe").unwrap();
        assert_eq!(v6.address, "[2620:fe::fe]:53".parse().unwrap());
        let doh = parse_server("https://dns.quad9.net/dns-query#9.9.9.9").unwrap();
        assert_eq!(doh.address, "9.9.9.9:443".parse().unwrap());
        assert_eq!(
            doh.kind,
            Kind::Https { host: "dns.quad9.net".into(), path: "/dns-query".into() }
        );
        let doh = parse_server("https://127.0.0.1:8443").unwrap();
        assert_eq!(doh.address, "127.0.0.1:8443".parse().unwrap());
        assert!(parse_server("https://dns.quad9.net/dns-query").is_err());
        assert!(parse_server("dns.quad9.net").is_err());
        assert!(parse_server("9.9.9.9#").is_err());
        assert!(parse_server("9.9.9.9:99999").is_err());
    }

    #[test]
    fn settings_read_with_their_comments_and_mistakes() {
        let (settings, wrong) = parse_settings(
            "# Quad9, over TLS\n\
             servers = 9.9.9.9#dns.quad9.net 149.112.112.112#dns.quad9.net # both\n\
             dns-over-tls = yes\n\
             cache-size = 10\n\
             certificates = /a.pem /b.pem\n\
             colour = blue\n\
             dns-over-tls = maybe\n",
        );
        assert_eq!(settings.servers.len(), 2);
        assert_eq!(settings.servers[1].kind, Kind::Tls("dns.quad9.net".into()));
        assert_eq!(settings.over_tls, OverTls::Yes);
        assert_eq!(settings.cache_size, 10);
        assert_eq!(settings.certificates, ["/a.pem", "/b.pem"]);
        assert_eq!(wrong.len(), 2, "{wrong:?}");
        assert!(wrong[0].ends_with(":6: no setting 'colour'"));
        let (settings, wrong) = parse_settings("");
        assert_eq!((settings.cache_size, settings.over_tls), (CACHE_DEFAULT, OverTls::Opportunistic));
        assert!(wrong.is_empty());
    }

    #[test]
    fn netd_s_resolv_conf_gives_the_servers_and_domains() {
        let (servers, search) = parse_resolv(
            "# Written by netd(8)\nnameserver 10.0.2.3\nnameserver 127.0.0.53\n\
             nameserver bogus\nsearch example.org lan\n",
        );
        assert_eq!(servers.len(), 1);
        assert!(servers[0].from_dhcp);
        assert_eq!(servers[0].address, "10.0.2.3:53".parse().unwrap());
        assert_eq!(search, ["example.org", "lan"]);
        assert!(stub_text(&search).contains("\nnameserver 127.0.0.53\n"));
        assert!(stub_text(&search).ends_with("search example.org lan\n"));
    }

    #[test]
    fn the_cache_keeps_ages_and_drops_answers() {
        let mut cache = Cache { size: 2, ..Cache::default() };
        let now = Instant::now();
        let key = |name: &str| Key {
            name: name.into(),
            qtype: dns::A,
            qclass: dns::IN,
            dnssec: false,
            unchecked: false,
        };
        let answer = |name| {
            let query = dns::query(name, dns::A, 1, 512).unwrap();
            let question = dns::question(&query).unwrap();
            dns::local_answer(&query, &question).unwrap()
        };
        cache.put(key("a"), &answer("localhost"), 60, now);
        cache.put(key("b"), &answer("localhost"), 30, now);
        assert!(cache.get(&key("a"), now + Duration::from_secs(59)).is_some());
        assert!(cache.get(&key("b"), now + Duration::from_secs(31)).is_none());
        cache.put(key("b"), &answer("localhost"), 30, now);
        // Full: the one closest to going makes room.
        cache.put(key("c"), &answer("localhost"), 90, now);
        assert_eq!(cache.entries.len(), 2);
        assert!(cache.get(&key("b"), now).is_none());
        // Nothing kept for no time, or with no room.
        cache.put(key("d"), &answer("localhost"), 0, now);
        assert!(cache.get(&key("d"), now).is_none());
    }

    #[test]
    fn http_answers_read_with_a_length_or_in_chunks() {
        let body = b"\x00\x01abc";
        let mut response = b"HTTP/1.1 200 OK\r\nContent-Type: application/dns-message\r\n\
                             Content-Length: 5\r\n\r\n"
            .to_vec();
        response.extend_from_slice(body);
        assert_eq!(http_response(&response[..]).unwrap(), (body.to_vec(), true));
        let chunked = b"HTTP/1.1 200 OK\r\ncontent-type: application/dns-message\r\n\
                        Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
                        2\r\n\x00\x01\r\n3\r\nabc\r\n0\r\n\r\n";
        assert_eq!(http_response(&chunked[..]).unwrap(), (body.to_vec(), false));
        let refused = b"HTTP/1.1 415 Unsupported\r\nContent-Length: 0\r\n\r\n";
        assert!(http_response(&refused[..]).is_err());
    }
}
