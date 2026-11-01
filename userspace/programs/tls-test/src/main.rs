// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The test of TLS on Radix (the `tls` boot test, ADR 0192): ring's
//! primitives against answers OpenSSL gave, rustls's handshakes over lo
//! with each protocol version, cipher suite, key exchange group and kind
//! of key it offers, the certificates it must refuse, and the root
//! certificates the system trusts. Like the C tests, it prints
//! "tls-test: <part>" for each part that passed, a line for each check
//! that fails, and "tls-test: every check passed"; it exits with how many
//! checks failed.
//!
//! The certificates, keys and signatures in testdata/ are OpenSSL's
//! (testdata/make.sh), so that another implementation than the one under
//! test made them, and the sealed messages' hashes in testdata/aead.txt
//! are Python's cryptography package's (testdata/aead.py).

use std::fmt::Debug;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use ring::{aead, digest, hkdf, hmac};
use rustls::crypto::{CryptoProvider, SupportedKxGroup, ring as provider};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, ServerName, SignatureVerificationAlgorithm,
};
use rustls::server::WebPkiClientVerifier;
use rustls::{
    ClientConfig, ClientConnection, Error, RootCertStore, ServerConfig, ServerConnection,
    StreamOwned, SupportedCipherSuite, SupportedProtocolVersion,
};

/// The test's name, which every line it prints starts with.
const NAME: &str = "tls-test";

/// The root certificates the system trusts (dukovo-certificates(7)).
const ROOTS: &str = "/system/resources/certificates/ca-certificates.crt";

/// The names of [`ROOTS`] that programs from Linux look for, in
/// `/etc`, which is `/system/settings`.
const ROOT_NAMES: [&str; 2] =
    ["/system/settings/ssl/certs/ca-certificates.crt", "/system/settings/ssl/cert.pem"];

/// The name the test servers' certificates are for, beside 127.0.0.1.
const SERVER: &str = "tls.test";

/// How much the bulk part sends each way.
const BULK: usize = 4 << 20;

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

fn main() {
    known_answers();
    signatures();
    handshakes();
    bulk();
    refusals();
    roots();
    let failures = FAILURES.load(Ordering::Relaxed);
    if failures == 0 {
        println!("{NAME}: every check passed");
    } else {
        println!("{NAME}: {failures} checks failed");
    }
    std::process::exit(i32::try_from(failures).unwrap_or(i32::MAX));
}

/// Lowercase hexadecimal.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// The bytes of hexadecimal text.
fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap_or(0))
        .collect()
}

/// The hashes, MACs, key derivation and sealing of TLS against answers
/// from their standards and from OpenSSL.
fn known_answers() {
    // FIPS 180-2's examples.
    let abc = [
        (&digest::SHA256, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
        (
            &digest::SHA384,
            "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7",
        ),
        (
            &digest::SHA512,
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
        ),
    ];
    for (algorithm, answer) in abc {
        let got = hex(digest::digest(algorithm, b"abc").as_ref());
        check!(got == answer, "{algorithm:?} of \"abc\" is {got}");
    }
    let mut context = digest::Context::new(&digest::SHA256);
    for _ in 0..1000 {
        context.update(&[b'a'; 1000]);
    }
    let million = hex(context.finish().as_ref());
    check!(
        million == "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0",
        "SHA-256 of a million a's is {million}"
    );
    // RFC 4231's second case.
    let key = hmac::Key::new(hmac::HMAC_SHA256, b"Jefe");
    let tag = hex(hmac::sign(&key, b"what do ya want for nothing?").as_ref());
    check!(
        tag == "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
        "HMAC-SHA256 is {tag}"
    );
    // RFC 5869's first case.
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, &unhex("000102030405060708090a0b0c"));
    let prk = salt.extract(&[0x0b; 22]);
    let info = unhex("f0f1f2f3f4f5f6f7f8f9");
    let mut okm = [0u8; 42];
    if let Some(okm_key) = ok!(prk.expand(&[&info], Okm(42))) {
        ok!(okm_key.fill(&mut okm));
    }
    check!(
        hex(&okm)
            == "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865",
        "HKDF-SHA256 gave {}",
        hex(&okm)
    );
    let mut sealed = 0;
    for line in String::from_utf8_lossy(testdata!("aead.txt")).lines() {
        let mut words = line.split(' ');
        let (Some(name), Some(length), Some(answer)) = (words.next(), words.next(), words.next())
        else {
            continue;
        };
        let algorithm = match name {
            "AES-128-GCM" => &aead::AES_128_GCM,
            "AES-256-GCM" => &aead::AES_256_GCM,
            "CHACHA20-POLY1305" => &aead::CHACHA20_POLY1305,
            _ => {
                check!(false, "aead.txt names {name}");
                continue;
            }
        };
        let length: usize = length.parse().unwrap_or(0);
        if seal_and_open(algorithm, length, answer) {
            sealed += 1;
        }
    }
    check!(sealed == 36, "{sealed} of aead.txt's 36 messages sealed and opened");
    println!(
        "{NAME}: known answers: SHA-256, SHA-384, SHA-512, HMAC, HKDF, and {sealed} messages sealed with AES-GCM and ChaCha20-Poly1305 as OpenSSL seals them"
    );
}

/// HKDF's output length, as ring asks for it.
struct Okm(usize);

impl hkdf::KeyType for Okm {
    fn len(&self) -> usize {
        self.0
    }
}

/// Seals aead.py's message of `length` bytes as aead.py does, checks the
/// hash of what it made against `answer`, opens it again, and checks that
/// a changed tag is refused.
fn seal_and_open(algorithm: &'static aead::Algorithm, length: usize, answer: &str) -> bool {
    let key_bytes: Vec<u8> = (0..32).collect();
    let nonce: [u8; 12] = std::array::from_fn(|i| 100 + u8::try_from(i).unwrap_or(0));
    let aad = aead::Aad::from(b"dukovo tls-test");
    let plain: Vec<u8> =
        (0..length).map(|i| (i.wrapping_mul(7).wrapping_add(3) & 0xff) as u8).collect();
    let Some(unbound) = ok!(aead::UnboundKey::new(algorithm, &key_bytes[..algorithm.key_len()]))
    else {
        return false;
    };
    let key = aead::LessSafeKey::new(unbound);
    let mut message = plain.clone();
    if ok!(key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(nonce),
        aad,
        &mut message
    ))
    .is_none()
    {
        return false;
    }
    let got = hex(digest::digest(&digest::SHA256, &message).as_ref());
    let mut right = got == answer;
    check!(right, "{algorithm:?} sealed {length} bytes as {got}, OpenSSL as {answer}");
    let mut opened = message.clone();
    match key.open_in_place(aead::Nonce::assume_unique_for_key(nonce), aad, &mut opened) {
        Ok(out) => {
            right &= out == plain.as_slice();
            check!(out == plain.as_slice(), "{algorithm:?} opened {length} bytes wrong");
        }
        Err(err) => {
            right = false;
            check!(false, "{algorithm:?} did not open {length} bytes: {err:?}");
        }
    }
    let last = message.len() - 1;
    message[last] ^= 1;
    let refused =
        key.open_in_place(aead::Nonce::assume_unique_for_key(nonce), aad, &mut message).is_err();
    check!(refused, "{algorithm:?} opened {length} bytes with a changed tag");
    right && refused
}

/// A signature to check: its name, the certificate whose key made it,
/// the signature, and how it is checked.
type Signed =
    (&'static str, &'static [u8], &'static [u8], &'static dyn SignatureVerificationAlgorithm);

/// OpenSSL's signatures of testdata/message.txt, checked with each
/// server certificate's key, and refused for another message.
fn signatures() {
    let message = testdata!("message.txt");
    let cases: [Signed; 5] = [
        (
            "ECDSA P-256",
            testdata!("ecdsa-p256.pem"),
            testdata!("ecdsa-p256.sig"),
            webpki::ring::ECDSA_P256_SHA256,
        ),
        (
            "ECDSA P-384",
            testdata!("ecdsa-p384.pem"),
            testdata!("ecdsa-p384.sig"),
            webpki::ring::ECDSA_P384_SHA384,
        ),
        ("Ed25519", testdata!("ed25519.pem"), testdata!("ed25519.sig"), webpki::ring::ED25519),
        (
            "RSA PKCS #1",
            testdata!("rsa-2048.pem"),
            testdata!("rsa-2048-pkcs1.sig"),
            webpki::ring::RSA_PKCS1_2048_8192_SHA256,
        ),
        (
            "RSA-PSS",
            testdata!("rsa-2048.pem"),
            testdata!("rsa-2048-pss.sig"),
            webpki::ring::RSA_PSS_2048_8192_SHA256_LEGACY_KEY,
        ),
    ];
    let mut checked = 0;
    for (what, pem, signature, algorithm) in cases {
        let Some(der) = ok!(CertificateDer::from_pem_slice(pem)) else { continue };
        let Some(cert) = ok!(webpki::EndEntityCert::try_from(&der)) else { continue };
        let good = cert.verify_signature(algorithm, message, signature);
        check!(good.is_ok(), "{what}: OpenSSL's signature refused: {good:?}");
        let bad = cert.verify_signature(algorithm, b"another message", signature);
        check!(bad.is_err(), "{what}: the signature passed for another message");
        if good.is_ok() && bad.is_err() {
            checked += 1;
        }
    }
    println!(
        "{NAME}: signatures: {checked} of OpenSSL's, ECDSA P-256 and P-384, Ed25519, RSA PKCS #1 and RSA-PSS, checked"
    );
}

/// A server's certificates and key, from testdata/.
struct Identity {
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
}

impl Identity {
    fn new(chain: &[&[u8]], key: &[u8]) -> Self {
        let chain =
            chain.iter().filter_map(|pem| ok!(CertificateDer::from_pem_slice(pem))).collect();
        let key = ok!(PrivateKeyDer::from_pem_slice(key))
            .unwrap_or_else(|| PrivateKeyDer::Pkcs8(Vec::new().into()));
        Self { chain, key }
    }

    fn ecdsa_p256() -> Self {
        Self::new(&[testdata!("ecdsa-p256.pem")], testdata!("ecdsa-p256.key"))
    }

    /// The only one that comes through the intermediate authority.
    fn ecdsa_p384() -> Self {
        Self::new(
            &[testdata!("ecdsa-p384.pem"), testdata!("intermediate.pem")],
            testdata!("ecdsa-p384.key"),
        )
    }

    fn ed25519() -> Self {
        Self::new(&[testdata!("ed25519.pem")], testdata!("ed25519.key"))
    }

    fn rsa_2048() -> Self {
        Self::new(&[testdata!("rsa-2048.pem")], testdata!("rsa-2048.key"))
    }
}

/// The test authority, as the clients and the server of the client
/// certificate trust it.
fn test_roots() -> Arc<RootCertStore> {
    let mut roots = RootCertStore::empty();
    if let Some(ca) = ok!(CertificateDer::from_pem_slice(testdata!("ca.pem"))) {
        ok!(roots.add(ca));
    }
    Arc::new(roots)
}

/// ring's provider, with only the suite and group given.
fn provider_with(
    suite: Option<SupportedCipherSuite>,
    group: Option<&'static dyn SupportedKxGroup>,
) -> Arc<CryptoProvider> {
    let mut provider = provider::default_provider();
    if let Some(suite) = suite {
        provider.cipher_suites = vec![suite];
    }
    if let Some(group) = group {
        provider.kx_groups = vec![group];
    }
    Arc::new(provider)
}

/// What one handshake asks for, and what the client presents.
struct Ask {
    version: &'static SupportedProtocolVersion,
    suite: Option<SupportedCipherSuite>,
    group: Option<&'static dyn SupportedKxGroup>,
    name: ServerName<'static>,
    roots: Arc<RootCertStore>,
    client_identity: Option<Identity>,
}

impl Ask {
    fn new(version: &'static SupportedProtocolVersion) -> Self {
        Self {
            version,
            suite: None,
            group: None,
            name: ServerName::try_from(SERVER).map_or_else(
                |_| ServerName::IpAddress(Ipv4Addr::LOCALHOST.into()),
                |name| name.to_owned(),
            ),
            roots: test_roots(),
            client_identity: None,
        }
    }
}

/// What a handshake gave each end.
#[derive(Debug)]
struct Done {
    version: Option<rustls::ProtocolVersion>,
    suite: Option<rustls::CipherSuite>,
    group: Option<rustls::NamedGroup>,
    /// What the server saw of the client's certificate.
    client_certificates: usize,
}

/// Runs a TLS server on lo for one connection, as `server` configures
/// it, and a client as `ask` says, which sends
/// "ping" and reads "pong". The client's error, or the server's when the
/// client saw none, is the result's.
fn handshake(
    server: impl FnOnce(Arc<CryptoProvider>) -> Result<ServerConfig, Error>,
    ask: Ask,
) -> Result<Done, Error> {
    let provider = provider_with(ask.suite, ask.group);
    let config = Arc::new(server(provider.clone())?);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .map_err(|err| Error::General(err.to_string()))?;
    let port = listener.local_addr().map_err(|err| Error::General(err.to_string()))?.port();
    let served = thread::spawn(move || -> Result<usize, Error> {
        let (socket, _) = listener.accept().map_err(|err| Error::General(err.to_string()))?;
        limit(&socket)?;
        let connection = ServerConnection::new(config)?;
        let mut stream = StreamOwned::new(connection, socket);
        let mut ping = [0u8; 4];
        stream.read_exact(&mut ping).map_err(|err| io_error(&err))?;
        if &ping != b"ping" {
            return Err(Error::General(format!("the server read {ping:?}")));
        }
        stream.write_all(b"pong").map_err(|err| io_error(&err))?;
        stream.flush().map_err(|err| io_error(&err))?;
        let seen = stream.conn.peer_certificates().map_or(0, <[_]>::len);
        stream.conn.send_close_notify();
        let _ = stream.flush();
        Ok(seen)
    });
    let builder = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[ask.version])?
        .with_root_certificates(ask.roots);
    let client = match ask.client_identity {
        Some(identity) => builder.with_client_auth_cert(identity.chain, identity.key)?,
        None => builder.with_no_client_auth(),
    };
    let result = (|| {
        let socket = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .map_err(|err| Error::General(err.to_string()))?;
        limit(&socket)?;
        let connection = ClientConnection::new(Arc::new(client), ask.name)?;
        let mut stream = StreamOwned::new(connection, socket);
        stream.write_all(b"ping").map_err(|err| io_error(&err))?;
        stream.flush().map_err(|err| io_error(&err))?;
        let mut pong = [0u8; 4];
        stream.read_exact(&mut pong).map_err(|err| io_error(&err))?;
        if &pong != b"pong" {
            return Err(Error::General(format!("the client read {pong:?}")));
        }
        Ok(Done {
            version: stream.conn.protocol_version(),
            suite: stream.conn.negotiated_cipher_suite().map(|suite| suite.suite()),
            group: stream.conn.negotiated_key_exchange_group().map(SupportedKxGroup::name),
            client_certificates: 0,
        })
    })();
    if result.is_err() {
        // A client that failed before it connected leaves the server
        // waiting for a connection: give it one, which ends at once.
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, port));
    }
    let served =
        served.join().unwrap_or_else(|_| Err(Error::General("the server panicked".into())));
    match (result, served) {
        (Ok(mut done), Ok(seen)) => {
            done.client_certificates = seen;
            Ok(done)
        }
        (Err(err), _) | (Ok(_), Err(err)) => Err(err),
    }
}

/// How long an end waits for the other before it gives up, so that a
/// handshake that stalls fails its check instead of the test.
const PATIENCE: std::time::Duration = std::time::Duration::from_secs(60);

/// Gives `socket` [`PATIENCE`] for each read and write.
fn limit(socket: &TcpStream) -> Result<(), Error> {
    socket
        .set_read_timeout(Some(PATIENCE))
        .and_then(|()| socket.set_write_timeout(Some(PATIENCE)))
        .map_err(|err| Error::General(format!("timeouts: {err}")))
}

/// rustls's error inside an I/O error, or the I/O error as rustls's.
fn io_error(err: &std::io::Error) -> Error {
    err.get_ref()
        .and_then(|inner| inner.downcast_ref::<Error>())
        .cloned()
        .unwrap_or_else(|| Error::General(err.to_string()))
}

/// A server that takes the client's version, with `identity`'s
/// certificates.
fn plain_server(
    identity: &Identity,
) -> impl FnOnce(Arc<CryptoProvider>) -> Result<ServerConfig, Error> + use<> {
    let chain = identity.chain.clone();
    let key = identity.key.clone_key();
    move |provider| {
        ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(chain, key)
    }
}

/// Every protocol version, cipher suite and key exchange group rustls
/// offers on ring, and each kind of server key, over lo.
fn handshakes() {
    use provider::cipher_suite::{
        TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256, TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256, TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384, TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
        TLS13_AES_128_GCM_SHA256, TLS13_AES_256_GCM_SHA384, TLS13_CHACHA20_POLY1305_SHA256,
    };
    use provider::kx_group::{SECP256R1, SECP384R1, X25519};
    use rustls::version::{TLS12, TLS13};
    let mut passed = 0;
    let mut tried = 0;
    let mut try_one = |what: String, identity: Identity, ask: Ask| {
        tried += 1;
        let (version, suite, group) =
            (ask.version.version, ask.suite, ask.group.map(SupportedKxGroup::name));
        match handshake(plain_server(&identity), ask) {
            Ok(done) => {
                let right = done.version == Some(version)
                    && suite.is_none_or(|suite| done.suite == Some(suite.suite()))
                    && group.is_none_or(|group| done.group == Some(group));
                check!(
                    right,
                    "{what}: {:?}, {:?} and {:?} were negotiated",
                    done.version,
                    done.suite,
                    done.group
                );
                if right {
                    passed += 1;
                }
            }
            Err(err) => check!(false, "{what}: {err:?}"),
        }
    };
    for suite in
        [TLS13_AES_128_GCM_SHA256, TLS13_AES_256_GCM_SHA384, TLS13_CHACHA20_POLY1305_SHA256]
    {
        for group in [X25519, SECP256R1, SECP384R1] {
            let ask = Ask { suite: Some(suite), group: Some(group), ..Ask::new(&TLS13) };
            try_one(
                format!("TLS 1.3 {:?} {:?}", suite.suite(), group.name()),
                Identity::ecdsa_p256(),
                ask,
            );
        }
    }
    for (what, identity) in [
        ("ECDSA P-384", Identity::ecdsa_p384()),
        ("Ed25519", Identity::ed25519()),
        ("RSA", Identity::rsa_2048()),
    ] {
        try_one(format!("TLS 1.3 with an {what} key"), identity, Ask::new(&TLS13));
    }
    for suite in [
        TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
    ] {
        let ask = Ask { suite: Some(suite), group: Some(X25519), ..Ask::new(&TLS12) };
        try_one(format!("TLS 1.2 {:?}", suite.suite()), Identity::ecdsa_p256(), ask);
    }
    for suite in [
        TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
        TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
    ] {
        let ask = Ask { suite: Some(suite), group: Some(SECP256R1), ..Ask::new(&TLS12) };
        try_one(format!("TLS 1.2 {:?}", suite.suite()), Identity::rsa_2048(), ask);
    }
    let by_address =
        Ask { name: ServerName::IpAddress(Ipv4Addr::LOCALHOST.into()), ..Ask::new(&TLS13) };
    try_one("TLS 1.3 to 127.0.0.1 by its address".into(), Identity::ecdsa_p256(), by_address);
    println!(
        "{NAME}: handshakes: {passed} of {tried} over lo, each with the version, suite, group and key asked for"
    );
}

/// Four MiB each way through one TLS 1.3 connection, every byte checked.
fn bulk() {
    let identity = Identity::ecdsa_p256();
    let Some(config) = ok!(plain_server(&identity)(provider_with(None, None))) else { return };
    let config = Arc::new(config);
    let Some(listener) = ok!(TcpListener::bind((Ipv4Addr::LOCALHOST, 0))) else { return };
    let Some(port) = ok!(listener.local_addr()).map(|addr| addr.port()) else { return };
    let pattern = |seed: u8| move |i: usize| (i.wrapping_mul(31) as u8).wrapping_add(seed);
    let served = thread::spawn(move || -> Result<bool, String> {
        let (socket, _) = listener.accept().map_err(|err| err.to_string())?;
        limit(&socket).map_err(|err| err.to_string())?;
        let connection = ServerConnection::new(config).map_err(|err| err.to_string())?;
        let mut stream = StreamOwned::new(connection, socket);
        let mut got = vec![0u8; BULK];
        stream.read_exact(&mut got).map_err(|err| err.to_string())?;
        let right = got.iter().enumerate().all(|(i, &byte)| byte == pattern(1)(i));
        let back: Vec<u8> = (0..BULK).map(pattern(2)).collect();
        stream.write_all(&back).map_err(|err| err.to_string())?;
        stream.flush().map_err(|err| err.to_string())?;
        Ok(right)
    });
    let client = ClientConfig::builder_with_provider(provider_with(None, None))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map(|builder| builder.with_root_certificates(test_roots()).with_no_client_auth());
    let received = (|| -> Result<bool, String> {
        let client = client.map_err(|err| err.to_string())?;
        let name = ServerName::try_from(SERVER).map_err(|err| err.to_string())?;
        let socket =
            TcpStream::connect((Ipv4Addr::LOCALHOST, port)).map_err(|err| err.to_string())?;
        limit(&socket).map_err(|err| err.to_string())?;
        let connection =
            ClientConnection::new(Arc::new(client), name).map_err(|err| err.to_string())?;
        let mut stream = StreamOwned::new(connection, socket);
        let out: Vec<u8> = (0..BULK).map(pattern(1)).collect();
        stream.write_all(&out).map_err(|err| err.to_string())?;
        stream.flush().map_err(|err| err.to_string())?;
        let mut back = vec![0u8; BULK];
        stream.read_exact(&mut back).map_err(|err| err.to_string())?;
        Ok(back.iter().enumerate().all(|(i, &byte)| byte == pattern(2)(i)))
    })();
    if received.is_err() {
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, port));
    }
    let served = served.join().unwrap_or_else(|_| Err("the server panicked".into()));
    check!(received == Ok(true), "the client received {received:?}");
    check!(served == Ok(true), "the server received {served:?}");
    if received == Ok(true) && served == Ok(true) {
        println!("{NAME}: bulk: {} MiB went each way through one TLS 1.3 connection", BULK >> 20);
    }
}

/// Whether `result` failed with an error that `matches` says is the one.
fn refused<T: Debug>(
    what: &str,
    result: &Result<T, Error>,
    matches: impl Fn(&Error) -> bool,
) -> bool {
    match result {
        Err(err) if matches(err) => true,
        other => {
            check!(false, "{what}: {other:?}");
            false
        }
    }
}

/// What a client must refuse, and a server that asks for the client's
/// certificate.
fn refusals() {
    use rustls::version::{TLS12, TLS13};
    let invalid = |want: &'static [&'static str]| move |err: &Error| matches!(err, Error::InvalidCertificate(cert) if want.iter().any(|name| format!("{cert:?}").starts_with(name)));
    let mut count = 0;
    let other_name = Ask {
        name: ServerName::try_from("other.test")
            .map_or_else(|_| ServerName::IpAddress(Ipv4Addr::BROADCAST.into()), |n| n.to_owned()),
        ..Ask::new(&TLS13)
    };
    let result = handshake(plain_server(&Identity::ecdsa_p256()), other_name);
    count += usize::from(refused("another name", &result, invalid(&["NotValidForName"])));
    let untrusted = Identity::new(&[testdata!("untrusted.pem")], testdata!("untrusted.key"));
    let result = handshake(plain_server(&untrusted), Ask::new(&TLS13));
    count += usize::from(refused("an unknown authority", &result, invalid(&["UnknownIssuer"])));
    let expired = Identity::new(&[testdata!("expired.pem")], testdata!("expired.key"));
    let result = handshake(plain_server(&expired), Ask::new(&TLS13));
    count += usize::from(refused("an expired certificate", &result, invalid(&["Expired"])));
    // The intermediate's certificate left out: the chain does not reach
    // the authority.
    let alone = Identity::new(&[testdata!("ecdsa-p384.pem")], testdata!("ecdsa-p384.key"));
    let result = handshake(plain_server(&alone), Ask::new(&TLS13));
    count += usize::from(refused(
        "a chain without its intermediate",
        &result,
        invalid(&["UnknownIssuer"]),
    ));
    // A server that speaks only TLS 1.2 to a client of TLS 1.3 alone.
    let identity = Identity::ecdsa_p256();
    let (chain, key) = (identity.chain, identity.key);
    let only_12 = move |provider: Arc<CryptoProvider>| {
        ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&TLS12])?
            .with_no_client_auth()
            .with_single_cert(chain, key)
    };
    let result = handshake(only_12, Ask::new(&TLS13));
    count += usize::from(refused("TLS 1.2 to a client of TLS 1.3", &result, |_| true));
    // The client's certificate, asked for and then given.
    let asks_for_client = |identity: &Identity| {
        let (chain, key) = (identity.chain.clone(), identity.key.clone_key());
        move |provider: Arc<CryptoProvider>| {
            let verifier =
                WebPkiClientVerifier::builder_with_provider(test_roots(), provider.clone())
                    .build()
                    .map_err(|err| Error::General(err.to_string()))?;
            ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()?
                .with_client_cert_verifier(verifier)
                .with_single_cert(chain, key)
        }
    };
    let result = handshake(asks_for_client(&Identity::ecdsa_p256()), Ask::new(&TLS13));
    count += usize::from(refused("no client certificate", &result, |_| true));
    let client = Identity::new(&[testdata!("client.pem")], testdata!("client.key"));
    let ask = Ask { client_identity: Some(client), ..Ask::new(&TLS13) };
    let result = handshake(asks_for_client(&Identity::ecdsa_p256()), ask);
    match result {
        Ok(done) if done.client_certificates == 1 => count += 1,
        Ok(done) => {
            check!(false, "the server saw {} client certificates", done.client_certificates);
        }
        Err(err) => check!(false, "a client certificate: {err:?}"),
    }
    // The system's roots do not trust the test authority.
    if let Some(system) = system_roots() {
        let ask = Ask { roots: Arc::new(system), ..Ask::new(&TLS13) };
        let result = handshake(plain_server(&Identity::ecdsa_p256()), ask);
        count += usize::from(refused(
            "the test authority by the system's roots",
            &result,
            invalid(&["UnknownIssuer"]),
        ));
    }
    println!(
        "{NAME}: refusals: {count} of 8: another name, an unknown authority, an expired certificate, a chain without its intermediate, TLS 1.2 alone, no client certificate where one is asked for, a client certificate given, and the system's roots for the test authority"
    );
}

/// The system's root certificates, as a store.
fn system_roots() -> Option<RootCertStore> {
    let certs: Vec<CertificateDer<'static>> =
        ok!(CertificateDer::pem_file_iter(ROOTS))?.filter_map(|cert| ok!(cert)).collect();
    let mut store = RootCertStore::empty();
    let (added, ignored) = store.add_parsable_certificates(certs.iter().cloned());
    check!(ignored == 0, "{ignored} of {} root certificates did not parse", certs.len());
    check!(added == certs.len(), "{added} of {} root certificates added", certs.len());
    Some(store)
}

/// The root certificates the system trusts, and Linux's names for them.
fn roots() {
    let Some(bytes) = ok!(std::fs::read(ROOTS)) else { return };
    let blocks = String::from_utf8_lossy(&bytes).matches("-----BEGIN CERTIFICATE-----").count();
    let Some(store) = system_roots() else { return };
    check!(store.len() == blocks, "{} of {blocks} certificates in {ROOTS}", store.len());
    check!(store.len() >= 100, "only {} root certificates", store.len());
    // Let's Encrypt's root, which most of the web's certificates come
    // from, is among them.
    let isrg = String::from_utf8_lossy(&bytes).contains("\n# ISRG Root X1\n");
    check!(isrg, "no ISRG Root X1 in {ROOTS}");
    for name in ROOT_NAMES {
        let target = ok!(std::fs::read_link(name));
        check!(
            target.as_deref() == Some(std::path::Path::new(ROOTS)),
            "{name} leads to {target:?}"
        );
        let same = ok!(std::fs::read(name));
        check!(same.as_deref() == Some(bytes.as_slice()), "{name} is not {ROOTS}");
    }
    println!(
        "{NAME}: roots: {} of Mozilla's root certificates in {ROOTS}, which /etc/ssl/certs/ca-certificates.crt and /etc/ssl/cert.pem lead to",
        store.len()
    );
}
