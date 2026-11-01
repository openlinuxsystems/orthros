// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The test of the network services (the `network-services` boot test,
//! ADRs 0193 to 0196): resolved(8), resolvectl(1), curl(1), timed(8),
//! timedatectl(1) and netlog(8), as the system runs them, against servers
//! this program runs on lo with the certificates of testdata/, which
//! OpenSSL made (testdata/make.sh):
//!
//! - DNS on port 5353, over UDP and TCP, for the names `*.test`;
//! - DNS over TLS on port 853 and over HTTPS on port 443, with the same
//!   answers, and a page for curl at `https://www.test/hello`;
//! - Network Time Security's key establishment on port 4460 and its NTP
//!   on port 123, whose time is five seconds ahead of the machine's;
//! - a log sink on UDP port 6666.
//!
//! It points the services at them by their settings, and checks what
//! they do. Like the C tests, it prints "netservices-test: <part>" for
//! each part that passed, a line for each check that fails, and
//! "netservices-test: every check passed"; it exits with how many checks
//! failed.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use netproto::dns;
use netproto::ntp::{self, KEY_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use rustls::crypto::ring as provider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

/// The test's name, which every line it prints starts with.
const NAME: &str = "netservices-test";

/// Where the test authority's certificate goes, for the services and
/// curl to trust.
const CA: &str = "/system/settings/netservices-ca.pem";

/// The services' settings, and what they say they did.
const RESOLVED: &str = "/system/settings/resolved";
const RESOLVED_STATUS: &str = "/system/live/resolved/status";
const TIMED: &str = "/system/settings/timed";
const TIMED_STATUS: &str = "/system/live/timed/status";
const NETLOG: &str = "/system/settings/netlog";

/// The ports the servers listen on, on 127.0.0.1: plain DNS's is not 53,
/// which resolved holds on 127.0.0.53.
const DNS_PORT: u16 = 5353;
const DOT_PORT: u16 = 853;
const HTTPS_PORT: u16 = 443;
const LOG_PORT: u16 = 6666;

/// How far ahead of the machine's clock the NTP server's is.
const AHEAD_NS: i128 = 5_000_000_000;

/// How long to wait for a service to do what it was asked.
const PATIENCE: Duration = Duration::from_secs(30);

/// How many checks have failed.
static FAILURES: AtomicUsize = AtomicUsize::new(0);

/// Says that a check failed, and counts it.
fn failed(file: &str, line: u32, what: &str) {
    println!("{NAME}: FAILED: {file}:{line}: {what}");
    FAILURES.fetch_add(1, Ordering::Relaxed);
}

/// Checks that a condition holds.
macro_rules! check {
    ($condition:expr) => {
        if !$condition {
            failed(file!(), line!(), stringify!($condition));
        }
    };
    ($condition:expr, $($what:tt)+) => {
        if !$condition {
            failed(file!(), line!(), &format!($($what)+));
        }
    };
}

/// The value of a result, or a failed check and `None`.
macro_rules! ok {
    ($result:expr) => {
        match $result {
            Ok(value) => Some(value),
            Err(err) => {
                failed(file!(), line!(), &format!("{}: {err:?}", stringify!($result)));
                None
            }
        }
    };
}

/// A file of testdata/.
macro_rules! testdata {
    ($name:literal) => {
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/", $name)).as_slice()
    };
}

/// Runs a part of the test, and says it passed if no check of it failed.
fn part(what: &str, run: impl FnOnce()) {
    let before = FAILURES.load(Ordering::Relaxed);
    run();
    if FAILURES.load(Ordering::Relaxed) == before {
        println!("{NAME}: {what}");
    } else {
        println!("{NAME}: FAILED: {what}");
    }
}

/// What the servers were asked: by whom, and what.
#[derive(Default)]
struct Seen {
    /// Each DNS question: the transport ("udp", "tcp", "tls", "https"),
    /// the name and the type.
    questions: Vec<(&'static str, String, u16)>,
    /// The paths of the HTTPS requests other than DNS's.
    pages: Vec<String>,
    /// The NTS key establishments, and the NTP requests answered.
    established: usize,
    times: usize,
    /// The log's datagrams.
    log: Vec<String>,
}

impl Seen {
    /// How many times `name` was asked over `transport` (any if empty).
    fn asked(&self, transport: &str, name: &str) -> usize {
        self.questions
            .iter()
            .filter(|(how, asked, _)| (transport.is_empty() || *how == transport) && asked == name)
            .count()
    }
}

/// What the servers share.
struct Servers {
    seen: Mutex<Seen>,
    /// The NTS cookies given, with the keys each stands for: client to
    /// server, and server to client.
    cookies: Mutex<HashMap<Vec<u8>, ([u8; KEY_LEN], [u8; KEY_LEN])>>,
    random: SystemRandom,
    /// The NTP server's time: the machine's when the test started, and
    /// five seconds, on from then by the monotonic clock.
    started_ns: i128,
    started: Instant,
    tls: Arc<ServerConfig>,
    https: Arc<ServerConfig>,
    nts: Arc<ServerConfig>,
}

impl Servers {
    fn seen(&self) -> std::sync::MutexGuard<'_, Seen> {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn random<const N: usize>(&self) -> [u8; N] {
        let mut bytes = [0; N];
        let _ = self.random.fill(&mut bytes);
        bytes
    }

    /// The NTP server's time, in nanoseconds since 1970.
    fn now_ns(&self) -> u64 {
        let ns = self.started_ns + AHEAD_NS + self.started.elapsed().as_nanos() as i128;
        u64::try_from(ns).unwrap_or(0)
    }
}

fn wall_ns() -> i128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i128)
}

/// The servers' TLS settings: the versions rustls takes by default, or
/// `versions`, offering `alpn`.
fn server_config(
    versions: &[&'static rustls::SupportedProtocolVersion],
    alpn: &[u8],
) -> Option<Arc<ServerConfig>> {
    let chain: Vec<CertificateDer<'static>> = vec![ok!(CertificateDer::from_pem_slice(testdata!(
        "server.pem"
    )))?];
    let key = ok!(PrivateKeyDer::from_pem_slice(testdata!("server.key")))?;
    let builder = ServerConfig::builder_with_provider(Arc::new(provider::default_provider()));
    let mut config = ok!(ok!(builder.with_protocol_versions(versions))?
        .with_no_client_auth()
        .with_single_cert(chain, key))?;
    config.alpn_protocols = vec![alpn.to_vec()];
    Some(Arc::new(config))
}

fn main() {
    let Some(servers) = start() else {
        println!("{NAME}: the servers did not start");
        std::process::exit(1);
    };
    part("plain DNS: names, the cache, NXDOMAIN, answers too long for UDP, localhost and .local, through resolved", || plain_dns(&servers));
    part("DNS over TLS: names asked over TLS, a certificate for another name refused", || over_tls(&servers));
    part("DNS over HTTPS: names asked by POST to https://dns.test/dns-query", || over_https(&servers));
    part("opportunistic DNS over TLS: a server named by address asked on port 853 first", || opportunistic(&servers));
    part("curl: https://www.test/hello fetched, an untrusted certificate refused", || curl(&servers));
    part("timed: the clock stepped 5 s by NTS, then left as it was; timedatectl shows it", || time(&servers));
    part("time zones: timedatectl set-timezone, list-timezones, and refusals", time_zones);
    part("netlog: the boot's first records and a new one sent to 127.0.0.1:6666", || netlog(&servers));
    let failures = FAILURES.load(Ordering::Relaxed);
    if failures == 0 {
        println!("{NAME}: every check passed");
    } else {
        println!("{NAME}: {failures} checks failed");
    }
    std::process::exit(i32::try_from(failures).unwrap_or(i32::MAX));
}

/// Writes the test authority's certificate, and starts every server.
fn start() -> Option<Arc<Servers>> {
    ok!(fs::write(CA, testdata!("ca.pem")))?;
    let tls13: &[&'static rustls::SupportedProtocolVersion] = &[&rustls::version::TLS13];
    let both: &[&'static rustls::SupportedProtocolVersion] = rustls::DEFAULT_VERSIONS;
    let servers = Arc::new(Servers {
        seen: Mutex::default(),
        cookies: Mutex::default(),
        random: SystemRandom::new(),
        started_ns: wall_ns(),
        started: Instant::now(),
        tls: server_config(both, b"dot")?,
        https: server_config(both, b"http/1.1")?,
        nts: server_config(tls13, ntp::NTS_ALPN)?,
    });
    let local = |port| SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let udp = ok!(UdpSocket::bind(local(DNS_PORT)))?;
    let tcp = ok!(TcpListener::bind(local(DNS_PORT)))?;
    let dot = ok!(TcpListener::bind(local(DOT_PORT)))?;
    let https = ok!(TcpListener::bind(local(HTTPS_PORT)))?;
    let ke = ok!(TcpListener::bind(local(ntp::NTS_KE_PORT)))?;
    let ntp_socket = ok!(UdpSocket::bind(local(ntp::NTP_PORT)))?;
    let sink = ok!(UdpSocket::bind(local(LOG_PORT)))?;
    let s = servers.clone();
    thread::spawn(move || dns_udp(&s, &udp));
    serve(&servers, tcp, |s, stream| dns_stream(s, stream, "tcp"));
    serve(&servers, dot, |s, stream| {
        if let Ok(connection) = ServerConnection::new(s.tls.clone()) {
            dns_stream(s, StreamOwned::new(connection, stream), "tls");
        }
    });
    serve(&servers, https, |s, stream| {
        if let Ok(connection) = ServerConnection::new(s.https.clone()) {
            https_stream(s, StreamOwned::new(connection, stream));
        }
    });
    serve(&servers, ke, |s, stream| {
        if let Ok(connection) = ServerConnection::new(s.nts.clone()) {
            let _ = key_establishment(s, StreamOwned::new(connection, stream));
        }
    });
    let s = servers.clone();
    thread::spawn(move || ntp_server(&s, &ntp_socket));
    let s = servers.clone();
    thread::spawn(move || log_sink(&s, &sink));
    Some(servers)
}

/// Serves each connection `listener` takes on a thread of its own.
fn serve(
    servers: &Arc<Servers>,
    listener: TcpListener,
    each: fn(&Arc<Servers>, TcpStream),
) {
    let servers = servers.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let servers = servers.clone();
            let _ = stream.set_read_timeout(Some(PATIENCE));
            let _ = stream.set_write_timeout(Some(PATIENCE));
            thread::spawn(move || each(&servers, stream));
        }
    });
}

/// The answer the test's zone gives `query`, and notes the question.
///
/// Every name under `test` has the address 127.0.0.1 and no IPv6 one,
/// but `missing.test`, which is not there; `big.test` and `huge.test`
/// have TXT records of 2 and 6 KiB, too long for UDP. Each negative
/// answer carries the zone's SOA, for five minutes.
fn zone_answer(servers: &Servers, query: &[u8], transport: &'static str) -> Option<Vec<u8>> {
    let question = dns::question(query)?;
    servers.seen().questions.push((transport, question.name.clone(), question.qtype));
    let name = question.name.as_str();
    if !name.ends_with(".test") && name != "test" {
        return Some(dns::failure(query, &question, dns::REFUSED));
    }
    if name == "missing.test" {
        return Some(with_soa(dns::failure(query, &question, dns::NXDOMAIN)));
    }
    let mut records: Vec<(u16, Vec<u8>)> = Vec::new();
    match (name, question.qtype) {
        ("big.test", dns::TXT) => records.extend((0..10).map(|n| (dns::TXT, txt(n)))),
        ("huge.test", dns::TXT) => records.extend((0..30).map(|n| (dns::TXT, txt(n)))),
        (_, dns::A) => records.push((dns::A, vec![127, 0, 0, 1])),
        _ => {}
    }
    if records.is_empty() {
        return Some(with_soa(dns::failure(query, &question, dns::NOERROR)));
    }
    Some(with_records(dns::failure(query, &question, dns::NOERROR), &records))
}

/// A TXT record's data of 200 bytes: one string of 199.
fn txt(n: usize) -> Vec<u8> {
    let mut data = vec![199];
    data.extend(format!("{n:03}").bytes());
    data.resize(200, b'x');
    data
}

/// `answer` with `records` in its answer section, each for its question's
/// name, for an hour.
fn with_records(mut answer: Vec<u8>, records: &[(u16, Vec<u8>)]) -> Vec<u8> {
    for (rtype, data) in records {
        answer.extend_from_slice(&[0xc0, dns::HEADER as u8]);
        answer.extend_from_slice(&rtype.to_be_bytes());
        answer.extend_from_slice(&dns::IN.to_be_bytes());
        answer.extend_from_slice(&3600u32.to_be_bytes());
        answer.extend_from_slice(&(data.len() as u16).to_be_bytes());
        answer.extend_from_slice(data);
    }
    let count = u16::try_from(records.len()).unwrap_or(u16::MAX);
    answer[6..8].copy_from_slice(&count.to_be_bytes());
    answer
}

/// `answer` with the zone's SOA in its authority section, whose minimum,
/// five minutes, is how long a negative answer is kept (RFC 2308).
fn with_soa(mut answer: Vec<u8>) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(b"\x03dns\x04test\x00\x05admin\x04test\x00");
    for value in [1u32, 3600, 600, 86_400, 300] {
        data.extend_from_slice(&value.to_be_bytes());
    }
    answer.extend_from_slice(b"\x04test\x00");
    answer.extend_from_slice(&dns::SOA.to_be_bytes());
    answer.extend_from_slice(&dns::IN.to_be_bytes());
    answer.extend_from_slice(&300u32.to_be_bytes());
    answer.extend_from_slice(&(data.len() as u16).to_be_bytes());
    answer.extend_from_slice(&data);
    answer[8..10].copy_from_slice(&1u16.to_be_bytes());
    answer
}

/// Plain DNS over UDP: an answer longer than the query offers is cut,
/// with TC set.
fn dns_udp(servers: &Servers, socket: &UdpSocket) {
    let mut buf = vec![0; 65_535];
    while let Ok((len, from)) = socket.recv_from(&mut buf) {
        let query = &buf[..len];
        if let Some(answer) = zone_answer(servers, query, "udp") {
            let _ = socket.send_to(&dns::fit(answer, dns::udp_size(query)), from);
        }
    }
}

/// DNS over a stream, TCP's or TLS's: queries and answers with their
/// lengths in front, until the client closes.
fn dns_stream<S: Read + Write>(servers: &Servers, mut stream: S, transport: &'static str) {
    loop {
        let mut len = [0; 2];
        if stream.read_exact(&mut len).is_err() {
            return;
        }
        let mut query = vec![0; usize::from(u16::from_be_bytes(len))];
        if stream.read_exact(&mut query).is_err() {
            return;
        }
        let Some(answer) = zone_answer(servers, &query, transport) else { return };
        let mut framed = (answer.len() as u16).to_be_bytes().to_vec();
        framed.extend_from_slice(&answer);
        if stream.write_all(&framed).and_then(|()| stream.flush()).is_err() {
            return;
        }
    }
}

/// HTTPS: DNS queries POSTed to /dns-query (RFC 8484), the page
/// /hello, and 404 for anything else, until the client closes.
fn https_stream<S: Read + Write>(servers: &Servers, stream: S) {
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut words = line.split_whitespace();
        let (method, path) = (words.next().unwrap_or("").to_string(), words.next().unwrap_or("").to_string());
        let mut length = 0;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                return;
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let (status, kind, content) = match (method.as_str(), path.as_str()) {
            ("POST", "/dns-query") => match zone_answer(servers, &body, "https") {
                Some(answer) => ("200 OK", "application/dns-message", answer),
                None => ("400 Bad Request", "text/plain", b"not a DNS query\n".to_vec()),
            },
            ("GET", _) => {
                servers.seen().pages.push(path.clone());
                if path == "/hello" {
                    ("200 OK", "text/plain", b"hello from www.test\n".to_vec())
                } else {
                    ("404 Not Found", "text/plain", b"no such page\n".to_vec())
                }
            }
            _ => ("405 Method Not Allowed", "text/plain", Vec::new()),
        };
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\r\n",
            content.len()
        )
        .into_bytes();
        response.extend_from_slice(&content);
        let stream = reader.get_mut();
        if stream.write_all(&response).and_then(|()| stream.flush()).is_err() {
            return;
        }
    }
}

/// NTS key establishment (RFC 8915): the client's records, then NTPv4,
/// AES-SIV-CMAC-256 and eight cookies, each standing for the keys the
/// TLS session exports.
fn key_establishment(
    servers: &Servers,
    mut stream: StreamOwned<ServerConnection, TcpStream>,
) -> io::Result<()> {
    let mut data = Vec::new();
    let mut buf = [0; 1024];
    let records = loop {
        if let Some((records, _)) = ntp::ke_records(&data) {
            break records;
        }
        let n = stream.read(&mut buf)?;
        if n == 0 || data.len() > 65_536 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        data.extend_from_slice(&buf[..n]);
    };
    let asks = |kind: u16, value: u16| {
        records.iter().any(|(k, _, body)| *k == kind && body.as_slice() == value.to_be_bytes())
    };
    if !asks(ntp::NEXT_PROTOCOL, ntp::PROTOCOL_NTPV4) || !asks(ntp::AEAD_ALGORITHM, ntp::AES_SIV_CMAC_256) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut c2s = [0; KEY_LEN];
    let mut s2c = [0; KEY_LEN];
    stream
        .conn
        .export_keying_material(&mut c2s, ntp::EXPORTER_LABEL, Some(&ntp::exporter_context(false)))
        .and_then(|_| {
            stream.conn.export_keying_material(
                &mut s2c,
                ntp::EXPORTER_LABEL,
                Some(&ntp::exporter_context(true)),
            )
        })
        .map_err(io::Error::other)?;
    let mut answer = ntp::record(ntp::NEXT_PROTOCOL, true, &ntp::PROTOCOL_NTPV4.to_be_bytes());
    answer.extend(ntp::record(ntp::AEAD_ALGORITHM, false, &ntp::AES_SIV_CMAC_256.to_be_bytes()));
    for _ in 0..8 {
        let cookie = new_cookie(servers, c2s, s2c);
        answer.extend(ntp::record(ntp::NEW_COOKIE, false, &cookie));
    }
    answer.extend(ntp::record(ntp::END_OF_MESSAGE, true, &[]));
    stream.write_all(&answer)?;
    stream.flush()?;
    servers.seen().established += 1;
    stream.conn.send_close_notify();
    stream.flush()
}

/// A new cookie for the keys `c2s` and `s2c`: random bytes the server
/// keeps them under.
fn new_cookie(servers: &Servers, c2s: [u8; KEY_LEN], s2c: [u8; KEY_LEN]) -> Vec<u8> {
    let cookie = servers.random::<32>().to_vec();
    servers.cookies.lock().unwrap_or_else(PoisonError::into_inner).insert(cookie.clone(), (c2s, s2c));
    cookie
}

/// NTS-protected NTP: a request is answered only if its cookie is one the
/// server gave and its Authenticator opens with the cookie's key; the
/// answer echoes its Unique Identifier and carries a new cookie for it
/// and one for each placeholder, sealed with the server's key.
fn ntp_server(servers: &Servers, socket: &UdpSocket) {
    let mut buf = vec![0; 2048];
    while let Ok((len, from)) = socket.recv_from(&mut buf) {
        let receive = ntp::timestamp(servers.now_ns());
        let request = &buf[..len];
        if let Some(answer) = nts_answer(servers, request, receive) {
            let _ = socket.send_to(&answer, from);
        }
    }
}

fn nts_answer(servers: &Servers, request: &[u8], receive: u64) -> Option<Vec<u8>> {
    let fields = ntp::fields(request)?;
    let body = |kind: u16| {
        fields.iter().find(|(k, _, _)| *k == kind).map(|&(_, at, end)| &request[at + 4..end])
    };
    let unique = body(ntp::UNIQUE_IDENTIFIER)?;
    let cookie = body(ntp::COOKIE)?;
    let &(_, at, end) = fields.iter().find(|(k, _, _)| *k == ntp::AUTHENTICATOR)?;
    let placeholders = fields.iter().filter(|(k, _, _)| *k == ntp::COOKIE_PLACEHOLDER).count();
    let (c2s, s2c) = servers.cookies.lock().unwrap_or_else(PoisonError::into_inner).remove(cookie)?;
    ntp::open(&c2s, request, at, end)?;
    let header = &request[..ntp::HEADER];
    let transmit = ntp::timestamp(servers.now_ns());
    let mut answer = ntp::server_answer(header, 2, receive, transmit, [0; 4]);
    answer.extend(ntp::field(ntp::UNIQUE_IDENTIFIER, unique));
    let mut plaintext = Vec::new();
    for _ in 0..=placeholders.min(7) {
        plaintext.extend(ntp::field(ntp::COOKIE, &new_cookie(servers, c2s, s2c)));
    }
    let nonce = servers.random::<16>();
    let sealed = ntp::authenticator(&s2c, &nonce, &answer, &plaintext)?;
    answer.extend(sealed);
    servers.seen().times += 1;
    Some(answer)
}

/// Keeps every datagram that comes to the log sink.
fn log_sink(servers: &Servers, socket: &UdpSocket) {
    let mut buf = vec![0; 2048];
    while let Ok((len, _)) = socket.recv_from(&mut buf) {
        servers.seen().log.push(String::from_utf8_lossy(&buf[..len]).into_owned());
    }
}

/// Runs a command, and returns what it wrote and how it ended.
fn run(program: &str, args: &[&str]) -> Option<Output> {
    ok!(Command::new(program).args(args).output())
}

/// What `program` wrote to its standard output, if it succeeded.
fn output(program: &str, args: &[&str]) -> String {
    let Some(out) = run(program, args) else { return String::new() };
    check!(
        out.status.success(),
        "{program} {}: {}: {}",
        args.join(" "),
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Has a service read its settings again: `service <name> reload`.
fn reload(service: &str) {
    output("/system/commands/service", &[service, "reload"]);
}

/// Waits until `ready` holds, or [`PATIENCE`] passes; whether it held.
fn wait_for(mut ready: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + PATIENCE;
    while Instant::now() < until {
        if ready() {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    ready()
}

/// Waits until `path` holds `text`.
fn wait_file(path: &str, text: &str) -> bool {
    let held = wait_for(|| fs::read_to_string(path).is_ok_and(|now| now.contains(text)));
    if !held {
        println!("{NAME}: {path} never said \"{text}\": {:?}", fs::read_to_string(path).unwrap_or_default());
    }
    held
}

/// Gives resolved `settings`, and waits until it shows `server`.
fn resolve_with(settings: &str, server: &str) {
    if ok!(fs::write(RESOLVED, settings)).is_none() {
        return;
    }
    reload("resolved");
    check!(wait_file(RESOLVED_STATUS, &format!("Server: {server} (from the settings)")));
}

/// The addresses the C library finds for `name`, through resolved.
fn lookup(name: &str) -> io::Result<Vec<SocketAddr>> {
    (name, 80).to_socket_addrs().map(Iterator::collect)
}

/// Whether `name` is found at 127.0.0.1.
fn found(name: &str) -> bool {
    match lookup(name) {
        Ok(addresses) => addresses.iter().any(|a| a.ip() == Ipv4Addr::LOCALHOST),
        Err(err) => {
            println!("{NAME}: {name}: {err}");
            false
        }
    }
}

fn plain_dns(servers: &Servers) {
    resolve_with(&format!("servers = 127.0.0.1:{DNS_PORT}\ndns-over-tls = no\n"), &format!("127.0.0.1:{DNS_PORT}"));
    check!(found("www.test"));
    check!(servers.seen().asked("udp", "www.test") >= 1);
    // Asked again, the answers come from the cache.
    check!(found("cached.test"));
    let first = servers.seen().asked("", "cached.test");
    check!(found("cached.test"));
    check!(servers.seen().asked("", "cached.test") == first, "cached.test asked again");
    // A name that is not there, and its negative answer kept too.
    check!(lookup("missing.test").is_err());
    let first = servers.seen().asked("", "missing.test");
    check!(lookup("missing.test").is_err());
    check!(servers.seen().asked("", "missing.test") == first, "missing.test asked again");
    // 2 KiB: past what resolved takes over UDP from a server, so asked
    // again over TCP; 6 KiB: past what resolvectl offers, so resolvectl
    // asks resolved again over TCP.
    let big = output("/system/commands/resolvectl", &["query", "-t", "TXT", "big.test"]);
    check!(big.contains("-- 10 records in "), "resolvectl query big.test: {big}");
    check!(servers.seen().asked("tcp", "big.test") >= 1);
    let huge = output("/system/commands/resolvectl", &["query", "-t", "TXT", "huge.test"]);
    check!(huge.contains("-- 30 records in "), "resolvectl query huge.test: {huge}");
    // localhost and .local never leave the machine.
    check!(found("localhost"));
    check!(lookup("printer.local").is_err());
    check!(servers.seen().asked("", "localhost") == 0 && servers.seen().asked("", "printer.local") == 0);
    // resolved writes its status every two seconds.
    check!(wait_file(RESOLVED_STATUS, "Listening on: 127.0.0.53:53 (UDP and TCP)"));
    check!(wait_file(RESOLVED_STATUS, "DNS over TLS: no"));
    // flush-caches empties the cache.
    output("/system/commands/resolvectl", &["flush-caches"]);
    let before = servers.seen().asked("", "cached.test");
    check!(wait_for(|| {
        found("cached.test");
        servers.seen().asked("", "cached.test") > before
    }));
    // /etc/resolv.conf leads to resolved's, which names it.
    let stub = fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    check!(stub.contains("nameserver 127.0.0.53\n"), "/etc/resolv.conf: {stub}");
}

fn over_tls(servers: &Servers) {
    resolve_with(&format!("servers = 127.0.0.1#dns.test\ncertificates = {CA}\n"), "127.0.0.1#dns.test");
    check!(found("dot.test"));
    check!(servers.seen().asked("tls", "dot.test") >= 1);
    check!(servers.seen().asked("udp", "dot.test") == 0 && servers.seen().asked("tcp", "dot.test") == 0);
    check!(wait_file(RESOLVED_STATUS, "DNS over TLS, its certificate checked"));
    // The certificate is not for wrong.test: nothing is asked.
    resolve_with(&format!("servers = 127.0.0.1#wrong.test\ncertificates = {CA}\n"), "127.0.0.1#wrong.test");
    check!(lookup("refused.test").is_err());
    check!(servers.seen().asked("", "refused.test") == 0);
    // Nor without the test authority.
    resolve_with("servers = 127.0.0.1#dns.test\n", "127.0.0.1#dns.test");
    check!(lookup("untrusted.test").is_err());
    check!(servers.seen().asked("", "untrusted.test") == 0);
}

fn over_https(servers: &Servers) {
    let server = "https://dns.test/dns-query#127.0.0.1";
    resolve_with(&format!("servers = {server}\ncertificates = {CA}\n"), server);
    check!(found("doh.test"));
    check!(servers.seen().asked("https", "doh.test") >= 1);
    check!(wait_file(RESOLVED_STATUS, &format!("Server: {server} (from the settings): DNS over HTTPS")));
}

fn opportunistic(servers: &Servers) {
    resolve_with(&format!("servers = 127.0.0.1:{DNS_PORT}\n"), &format!("127.0.0.1:{DNS_PORT}"));
    check!(found("opportunistic.test"));
    check!(servers.seen().asked("tls", "opportunistic.test") >= 1);
    check!(wait_file(RESOLVED_STATUS, "DNS over TLS, any certificate (opportunistic)"));
    check!(wait_file(RESOLVED_STATUS, "DNS over TLS: opportunistic"));
}

fn curl(servers: &Servers) {
    let version = output("/system/commands/curl", &["-V"]);
    check!(version.contains("rustls-ffi/") && version.contains("zlib/") && version.contains("zstd/"), "curl -V: {version}");
    let page = output("/system/commands/curl", &["-sS", "--cacert", CA, "https://www.test/hello"]);
    check!(page == "hello from www.test\n", "curl https://www.test/hello: {page:?}");
    check!(servers.seen().pages.iter().any(|p| p == "/hello"));
    // Without the test authority the handshake fails: no page is asked.
    if let Some(out) = run("/system/commands/curl", &["-sS", "https://www.test/untrusted"]) {
        let stderr = String::from_utf8_lossy(&out.stderr);
        check!(!out.status.success() && stderr.contains("curl: ("), "curl without --cacert: {}: {stderr}", out.status);
    }
    check!(!servers.seen().pages.iter().any(|p| p == "/untrusted"));
}

fn time(servers: &Servers) {
    // Names resolve by plain DNS from here on.
    resolve_with(&format!("servers = 127.0.0.1:{DNS_PORT}\ndns-over-tls = no\n"), &format!("127.0.0.1:{DNS_PORT}"));
    let (wall, mono) = (wall_ns(), Instant::now());
    if ok!(fs::write(TIMED, format!("servers = time.test\nfallback =\ncertificates = {CA}\n"))).is_none() {
        return;
    }
    reload("timed");
    check!(wait_file(TIMED_STATUS, "from time.test over NTS"));
    let moved = wall_ns() - wall - mono.elapsed().as_nanos() as i128;
    check!((moved - AHEAD_NS).abs() < 500_000_000, "the clock moved {moved} ns, not 5 s");
    let status = fs::read_to_string(TIMED_STATUS).unwrap_or_default();
    check!(status.contains("stepped the clock +"), "{status}");
    check!(servers.seen().established >= 1 && servers.seen().times >= 4);
    // A second round finds the clock right.
    let times = servers.seen().times;
    reload("timed");
    check!(wait_for(|| servers.seen().times >= times + 4));
    check!(wait_file(TIMED_STATUS, "left as it is") || wait_file(TIMED_STATUS, "slewing the clock"));
    let shown = output("/system/commands/timedatectl", &[]);
    check!(shown.contains("System clock synchronized: yes") && shown.contains("NTP service: active"), "{shown}");
    let sync = output("/system/commands/timedatectl", &["timesync-status"]);
    check!(sync.contains("from time.test over NTS"), "{sync}");
    // The clock is timed's while it runs.
    if let Some(out) = run("/system/commands/timedatectl", &["set-time", "2030-01-01 00:00"]) {
        let stderr = String::from_utf8_lossy(&out.stderr);
        check!(!out.status.success() && stderr.contains("timed keeps the clock"), "{stderr}");
    }
}

fn time_zones() {
    let zones = output("/system/commands/timedatectl", &["list-timezones"]);
    check!(zones.lines().count() > 300, "{} zones", zones.lines().count());
    check!(zones.lines().any(|z| z == "Europe/Berlin") && zones.lines().any(|z| z == "UTC"));
    output("/system/commands/timedatectl", &["set-timezone", "Europe/Berlin"]);
    let target = fs::read_link("/etc/localtime").unwrap_or_default();
    check!(target.to_string_lossy() == "/system/resources/zoneinfo/Europe/Berlin", "{}", target.display());
    let shown = output("/system/commands/timedatectl", &[]);
    check!(shown.contains("Time zone: Europe/Berlin (CE"), "{shown}");
    let zone = output("/system/commands/date", &["+%Z"]);
    check!(zone == "CET\n" || zone == "CEST\n", "date +%Z: {zone:?}");
    for wrong in ["Nowhere/Atlantis", "../../settings/passwd", "/system/settings/passwd"] {
        if let Some(out) = run("/system/commands/timedatectl", &["set-timezone", wrong]) {
            check!(!out.status.success(), "set-timezone {wrong}");
        }
    }
    output("/system/commands/timedatectl", &["set-timezone", "UTC"]);
    let zone = output("/system/commands/date", &["+%Z"]);
    check!(zone == "UTC\n", "date +%Z: {zone:?}");
}

fn netlog(servers: &Servers) {
    if ok!(fs::write(NETLOG, format!("target = 127.0.0.1:{LOG_PORT}\n"))).is_none() {
        return;
    }
    // From the oldest record the kernel still holds, the boot's first
    // unless its 512 records have been passed.
    let oldest = oldest_record();
    check!(!oldest.is_empty());
    reload("netlog");
    check!(wait_for(|| servers.seen().log.iter().any(|d| d.contains(oldest.as_str()))), "{oldest:?}");
    let marker = "netservices-test was here";
    output("/system/commands/logger", &[marker]);
    check!(wait_for(|| servers.seen().log.iter().any(|d| d.contains(marker))));
    let seen = servers.seen();
    check!(seen.log.iter().all(|d| d.starts_with('<') && d.contains(">[") && d.ends_with('\n')));
    if let Some(line) = seen.log.iter().find(|d| d.contains(marker)) {
        // logger's user.notice, which the kernel keeps as information:
        // it has four levels (logger(1)).
        check!(line.starts_with("<14>["), "logger's line: {line:?}");
    }
}

/// The text of the oldest record `/devices/kmsg` gives.
fn oldest_record() -> String {
    let mut buffer = vec![0u8; 8192];
    let Some(n) = fs::File::open("/devices/kmsg").and_then(|mut kmsg| kmsg.read(&mut buffer)).ok() else {
        return String::new();
    };
    let record = String::from_utf8_lossy(&buffer[..n]);
    let text = record.split_once(';').map_or("", |(_, text)| text);
    text.lines().next().unwrap_or("").to_string()
}
