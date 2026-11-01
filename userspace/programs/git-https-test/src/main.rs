// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The test of git over `https://` (the `git-https` boot test, ADR 0292).
//! It runs a smart HTTP server for git on lo, port 8443, with the test
//! authority's certificate from netservices-test's testdata/: TLS by
//! rustls, Basic sign-in as `test` with the token `s3cret`, and git's
//! own git-http-backend behind it as a CGI program, as a web server runs
//! it. Then it runs git as a person does:
//!
//! - a certificate from an authority git does not trust is refused;
//! - without a token, git asks for one, and with no terminal to ask on,
//!   fails, as does a wrong token;
//! - with the token given once (`git credential approve`), git
//!   credential-cache, the helper the system's gitconfig names, keeps it
//!   in memory: git clones, commits and pushes without asking, and the
//!   server's repository has the commit;
//! - the token is in no file, and once the cache is told to exit, git
//!   asks again.
//!
//! Like the C tests, it prints "git-https-test: <part>" for each part
//! that passed, a line for each check that fails, and "git-https-test:
//! every check passed"; it exits with how many checks failed.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use rustls::crypto::ring as provider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

/// The test's name, which every line it prints starts with.
const NAME: &str = "git-https-test";

/// Where the test's files go: the server's repositories, the user's
/// folder and the clone.
const DIR: &str = "/tmp/git-https";

/// The server's port on 127.0.0.1, and the repository's address.
const PORT: u16 = 8443;
const URL: &str = "https://127.0.0.1:8443/repo.git";

/// The one account the server lets in, and its token.
const USER: &str = "test";
const TOKEN: &str = "s3cret";
/// `Basic` and base64 of "test:s3cret".
const AUTHORIZATION: &str = "Basic dGVzdDpzM2NyZXQ=";

/// git's HTTP backend, which the server runs for each request.
const BACKEND: &str = "/system/libraries/git-core/git-http-backend";

/// How long the server waits on a quiet connection.
const PATIENCE: Duration = Duration::from_secs(60);

/// How many checks have failed.
static FAILURES: AtomicUsize = AtomicUsize::new(0);

/// Says that a check failed, and counts it.
fn failed(file: &str, line: u32, what: &str) {
    println!("{NAME}: FAILED: {file}:{line}: {what}");
    FAILURES.fetch_add(1, Ordering::Relaxed);
}

/// Checks that a condition holds, and says which did not.
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

/// The value of a result, or a failed check that names it.
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

/// A file of netservices-test's testdata/.
macro_rules! testdata {
    ($name:literal) => {
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../netservices-test/testdata/", $name))
            .as_slice()
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

/// A request the server answered: its method, path and status, and
/// whether it was signed in.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    status: u16,
    signed_in: bool,
}

/// What the server saw.
static SEEN: Mutex<Vec<Seen>> = Mutex::new(Vec::new());

fn seen() -> Vec<Seen> {
    SEEN.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

fn main() {
    let Some(()) = start() else {
        println!("{NAME}: the server did not start");
        std::process::exit(1);
    };
    part("git: built with libcurl on rustls, credential-cache the system's helper", built);
    part("untrusted: a certificate from an unknown authority refused", untrusted);
    part("no token: git asks, and fails with no terminal; a wrong token refused", refused);
    let mut pushed = String::new();
    part("token: kept in memory by credential-cache; clone, commit and push over https", || {
        pushed = with_token();
    });
    if !pushed.is_empty() {
        println!("{NAME}: pushed {pushed}");
    }
    part("forgotten: the token in no file, and asked for again once the cache exits", forgotten);
    let failures = FAILURES.load(Ordering::Relaxed);
    if failures == 0 {
        println!("{NAME}: every check passed");
    } else {
        println!("{NAME}: {failures} checks failed");
    }
    std::process::exit(i32::try_from(failures).unwrap_or(i32::MAX));
}

/// Makes the server's repository, with one commit, and starts the server.
fn start() -> Option<()> {
    let _ = fs::remove_dir_all(DIR);
    ok!(fs::create_dir_all(format!("{DIR}/home")))?;
    ok!(fs::create_dir_all(format!("{DIR}/srv")))?;
    ok!(fs::write(format!("{DIR}/ca.pem"), testdata!("ca.pem")))?;
    let seed = format!("{DIR}/seed");
    git(DIR, &["init", "-q", "-b", "main", "seed"])?;
    ok!(fs::write(format!("{seed}/file"), "from the server\n"))?;
    git(&seed, &["add", "file"])?;
    git(&seed, &["commit", "-q", "-m", "First"])?;
    git(DIR, &["clone", "-q", "--bare", "seed", "srv/repo.git"])?;
    // A push needs a signed-in user, which git-http-backend sees as
    // REMOTE_USER, and this.
    git(&format!("{DIR}/srv/repo.git"), &["config", "http.receivepack", "true"])?;

    let chain: Vec<CertificateDer<'static>> =
        vec![ok!(CertificateDer::from_pem_slice(testdata!("server.pem")))?];
    let key = ok!(PrivateKeyDer::from_pem_slice(testdata!("server.key")))?;
    let builder = ServerConfig::builder_with_provider(Arc::new(provider::default_provider()));
    let mut config = ok!(ok!(builder.with_safe_default_protocol_versions())?
        .with_no_client_auth()
        .with_single_cert(chain, key))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let config = Arc::new(config);
    let listener = ok!(TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, PORT))))?;
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let config = config.clone();
            let _ = stream.set_read_timeout(Some(PATIENCE));
            let _ = stream.set_write_timeout(Some(PATIENCE));
            thread::spawn(move || {
                if let Ok(connection) = ServerConnection::new(config) {
                    serve(StreamOwned::new(connection, stream));
                }
            });
        }
    });
    Some(())
}

/// A request's line, headers and body.
struct Request {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// Reads one request, or None when the client closed.
fn read_request(reader: &mut impl BufRead) -> Option<Request> {
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut words = line.split_whitespace();
    let method = words.next()?.to_string();
    let target = words.next()?.to_string();
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target, String::new()),
    };
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 {
            return None;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    let mut request = Request { method, path, query, headers, body: Vec::new() };
    let chunked =
        request.header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
    if chunked {
        // A push bigger than git's buffer comes in chunks.
        loop {
            let mut size = String::new();
            reader.read_line(&mut size).ok()?;
            let size = usize::from_str_radix(size.trim().split(';').next()?, 16).ok()?;
            let mut chunk = vec![0; size + 2];
            reader.read_exact(&mut chunk).ok()?;
            if size == 0 {
                break;
            }
            request.body.extend_from_slice(&chunk[..size]);
        }
    } else {
        let length = request.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
        request.body = vec![0; length];
        reader.read_exact(&mut request.body).ok()?;
    }
    Some(request)
}

/// Answers a connection's requests until the client closes it.
fn serve<S: Read + Write>(stream: S) {
    let mut reader = BufReader::new(stream);
    while let Some(request) = read_request(&mut reader) {
        let signed_in = request.header("authorization") == Some(AUTHORIZATION);
        let (status, response) = if signed_in {
            backend(&request)
        } else {
            let body = b"sign in, please\n";
            let head = format!(
                "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"git-https-test\"\r\n\
                 Content-Type: text/plain\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            (401, [head.as_bytes(), body].concat())
        };
        SEEN.lock().unwrap_or_else(PoisonError::into_inner).push(Seen {
            method: request.method.clone(),
            path: request.path.clone(),
            status,
            signed_in,
        });
        let stream = reader.get_mut();
        if stream.write_all(&response).and_then(|()| stream.flush()).is_err() {
            return;
        }
    }
}

/// Runs git-http-backend for a request, as CGI (RFC 3875), and makes
/// its answer an HTTP response.
fn backend(request: &Request) -> (u16, Vec<u8>) {
    let mut command = Command::new(BACKEND);
    command
        .env_clear()
        .env("PATH", "/system/commands")
        .env("GIT_PROJECT_ROOT", format!("{DIR}/srv"))
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("GATEWAY_INTERFACE", "CGI/1.1")
        .env("SERVER_PROTOCOL", "HTTP/1.1")
        .env("REQUEST_METHOD", &request.method)
        .env("PATH_INFO", &request.path)
        .env("QUERY_STRING", &request.query)
        .env("CONTENT_LENGTH", request.body.len().to_string())
        .env("REMOTE_USER", USER)
        .env("REMOTE_ADDR", "127.0.0.1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(kind) = request.header("content-type") {
        command.env("CONTENT_TYPE", kind);
    }
    if let Some(encoding) = request.header("content-encoding") {
        command.env("HTTP_CONTENT_ENCODING", encoding);
    }
    if let Some(protocol) = request.header("git-protocol") {
        command.env("GIT_PROTOCOL", protocol);
    }
    let error = |what: String| {
        let head = format!(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\n\
             Content-Length: {}\r\n\r\n",
            what.len()
        );
        (500, [head.into_bytes(), what.into_bytes()].concat())
    };
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return error(format!("{BACKEND}: {err}\n")),
    };
    // The body goes in while the answer comes out, so neither waits on
    // the other.
    let mut stdin = child.stdin.take();
    let body = request.body.clone();
    let writer = thread::spawn(move || {
        if let Some(stdin) = stdin.as_mut() {
            let _ = stdin.write_all(&body);
        }
    });
    let output = child.wait_with_output();
    let _ = writer.join();
    let Ok(output) = output else { return error(format!("{BACKEND} did not answer\n")) };
    let out = output.stdout;
    let (head, body) = match find(&out, b"\r\n\r\n") {
        Some(at) => (&out[..at], &out[at + 4..]),
        None => match find(&out, b"\n\n") {
            Some(at) => (&out[..at], &out[at + 2..]),
            None => return error(format!("{BACKEND} wrote no headers\n")),
        },
    };
    let mut status = 200;
    let mut reason = "OK".to_string();
    let mut headers = String::new();
    for line in String::from_utf8_lossy(head).lines() {
        let Some((name, value)) = line.split_once(':') else { continue };
        if name.eq_ignore_ascii_case("status") {
            let value = value.trim();
            let (code, text) = value.split_once(' ').unwrap_or((value, ""));
            status = code.parse().unwrap_or(500);
            reason = text.to_string();
        } else {
            headers.push_str(&format!("{}: {}\r\n", name.trim(), value.trim()));
        }
    }
    let head =
        format!("HTTP/1.1 {status} {reason}\r\n{headers}Content-Length: {}\r\n\r\n", body.len());
    (status, [head.as_bytes(), body].concat())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// git, as the user runs it: their folder, the test authority, the names
/// commits are made with, and no terminal to ask on.
fn git_command(dir: &str, args: &[&str]) -> Command {
    let mut command = Command::new("/system/commands/git");
    command
        .args(args)
        .current_dir(dir)
        .env("HOME", format!("{DIR}/home"))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_CACHE_HOME")
        .env("GIT_SSL_CAINFO", format!("{DIR}/ca.pem"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@dukovo")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@dukovo")
        .stdin(Stdio::null());
    command
}

/// What git wrote, whether it succeeded or not.
fn git_output(dir: &str, args: &[&str]) -> Option<Output> {
    ok!(git_command(dir, args).output())
}

/// git's standard output, if it succeeded, or a failed check.
fn git(dir: &str, args: &[&str]) -> Option<String> {
    let out = git_output(dir, args)?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        failed(
            file!(),
            line!(),
            &format!(
                "git {} in {dir}: {}: {}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        );
        None
    }
}

/// Gives git a credential, as a person's first sign-in does, for the
/// helpers to keep.
fn approve(token: &str) {
    let mut command = git_command(DIR, &["credential", "approve"]);
    command.stdin(Stdio::piped());
    let Some(mut child) = ok!(command.spawn()) else { return };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = write!(
            stdin,
            "protocol=https\nhost=127.0.0.1:{PORT}\nusername={USER}\npassword={token}\n\n"
        );
    }
    if let Some(status) = ok!(child.wait()) {
        check!(status.success(), "git credential approve: {status}");
    }
}

/// git was built with https, and the system names the memory cache.
fn built() {
    let version = git(DIR, &["version", "--build-options"]).unwrap_or_default();
    check!(version.contains("libcurl: 8.23.0"), "git version --build-options: {version}");
    let helper = git(DIR, &["config", "--get", "credential.helper"]).unwrap_or_default();
    check!(helper.starts_with("cache "), "credential.helper is {helper:?}");
    check!(Path::new("/system/libraries/git-core/git-remote-https").exists());
}

/// Without the test authority, the handshake fails before any request.
fn untrusted() {
    let before = seen().len();
    let Some(out) =
        ok!(git_command(DIR, &["ls-remote", URL]).env_remove("GIT_SSL_CAINFO").output())
    else {
        return;
    };
    let stderr = String::from_utf8_lossy(&out.stderr);
    check!(!out.status.success(), "git ls-remote without the authority succeeded");
    check!(stderr.contains("certificate") || stderr.contains("SSL"), "git said: {stderr}");
    check!(seen().len() == before, "the server answered a request: {:?}", seen());
}

/// No token, and a wrong one: the server says 401, and git fails.
fn refused() {
    let Some(out) = git_output(DIR, &["ls-remote", URL]) else { return };
    let stderr = String::from_utf8_lossy(&out.stderr);
    check!(!out.status.success(), "git ls-remote without a token succeeded");
    check!(
        stderr.contains("could not read Username") || stderr.contains("terminal prompts disabled"),
        "git said: {stderr}"
    );
    check!(seen().iter().any(|s| s.status == 401 && !s.signed_in), "{:?}", seen());
    approve("wrong");
    let Some(out) = git_output(DIR, &["ls-remote", URL]) else { return };
    let stderr = String::from_utf8_lossy(&out.stderr);
    check!(!out.status.success(), "git ls-remote with a wrong token succeeded");
    check!(stderr.contains("Authentication failed"), "git said: {stderr}");
    // git told the cache to forget the token that failed, so the next
    // try asks again rather than sending it.
    let Some(out) = git_output(DIR, &["ls-remote", URL]) else { return };
    let stderr = String::from_utf8_lossy(&out.stderr);
    check!(
        !out.status.success() && !stderr.contains("Authentication failed"),
        "after a refused token, git said: {stderr}"
    );
}

/// The token given once: clone, commit and push, and what the server's
/// repository holds then. Returns the pushed commit's hash.
fn with_token() -> String {
    approve(TOKEN);
    let Some(log) = git(DIR, &["clone", "-q", URL, "clone"])
        .and_then(|_| git(&format!("{DIR}/clone"), &["log", "--format=%s"]))
    else {
        return String::new();
    };
    check!(log == "First", "the clone's log: {log:?}");
    let clone = format!("{DIR}/clone");
    if ok!(fs::write(format!("{clone}/file"), "from the server\nfrom dukovo\n")).is_none() {
        return String::new();
    }
    let Some(_) = git(&clone, &["commit", "-q", "-am", "From Dukovo"]) else {
        return String::new();
    };
    let Some(_) = git(&clone, &["push", "-q", "origin", "main"]) else { return String::new() };
    let pushed = git(&clone, &["rev-parse", "HEAD"]).unwrap_or_default();
    let server = git(&format!("{DIR}/srv/repo.git"), &["rev-parse", "main"]).unwrap_or_default();
    check!(!pushed.is_empty() && server == pushed, "the server's main is {server}, not {pushed}");
    let subjects = git(&format!("{DIR}/srv/repo.git"), &["log", "--format=%s", "main"]);
    check!(subjects.as_deref() == Some("From Dukovo\nFirst"), "the server's log: {subjects:?}");
    // The push went through git-receive-pack, signed in.
    check!(
        seen().iter().any(|s| s.method == "POST"
            && s.path.ends_with("/git-receive-pack")
            && s.status == 200
            && s.signed_in),
        "{:?}",
        seen()
    );
    let fetched = git(&clone, &["fetch", "-q", "origin"]);
    check!(fetched.is_some());
    pushed
}

/// The token is in memory only: in no file of the user's, and gone once
/// the cache's daemon exits.
fn forgotten() {
    let mut files = Vec::new();
    walk(Path::new(&format!("{DIR}/home")), &mut files);
    walk(Path::new(&format!("{DIR}/clone/.git")), &mut files);
    for file in files {
        if let Ok(bytes) = fs::read(&file) {
            check!(find(&bytes, TOKEN.as_bytes()).is_none(), "{} holds the token", file.display());
        }
    }
    let _ = git(DIR, &["credential-cache", "exit"]);
    let Some(out) = git_output(DIR, &["ls-remote", URL]) else { return };
    check!(!out.status.success(), "git ls-remote after the cache exited succeeded");
}

/// Every file beneath `dir`, but not sockets.
fn walk(dir: &Path, files: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            walk(&entry.path(), files);
        } else if kind.is_file() {
            files.push(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_authorization_is_the_account_and_token() {
        // base64("test:s3cret"), worked out by hand: "tes" "t:s" "3cr"
        // "et" give dGVz dDpz M2Ny ZXQ=.
        assert_eq!(AUTHORIZATION, "Basic dGVzdDpzM2NyZXQ=");
        assert_eq!((USER, TOKEN), ("test", "s3cret"));
    }

    #[test]
    fn a_chunked_body_is_read_whole() {
        let raw = b"POST /repo.git/git-receive-pack HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n\
                    5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let request = read_request(&mut BufReader::new(raw.as_slice())).unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/repo.git/git-receive-pack");
        assert_eq!(request.body, b"hello world");
    }

    #[test]
    fn a_query_is_split_from_the_path() {
        let raw = b"GET /repo.git/info/refs?service=git-upload-pack HTTP/1.1\r\n\r\n";
        let request = read_request(&mut BufReader::new(raw.as_slice())).unwrap();
        assert_eq!(request.path, "/repo.git/info/refs");
        assert_eq!(request.query, "service=git-upload-pack");
        assert!(request.body.is_empty());
    }
}
