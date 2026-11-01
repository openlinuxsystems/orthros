// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! TLS for the network services, resolved(8) and timed(8): rustls on
//! ring (ADR 0192), with the root certificates the system trusts, as
//! `/etc/ssl/certs/ca-certificates.crt` names them (so that an
//! administrator's own file there counts), and any more a service's
//! settings name. Opportunistic DNS over TLS (RFC 7858's opportunistic
//! privacy profile, ADR 0193) takes any certificate the server holds the
//! key of: it hides the queries from those who only watch, not from one
//! who answers in the server's place.

use std::io::{self, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, ring};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, RootCertStore, StreamOwned};

/// The root certificates, by the name Linux's programs read them by, and
/// the system's own file, which that leads to unless an administrator
/// replaced it (dukovo-certificates(7)).
const ROOTS: [&str; 2] = [
    "/system/settings/ssl/certs/ca-certificates.crt",
    "/system/resources/certificates/ca-certificates.crt",
];

/// A TLS connection over TCP.
pub type Stream = StreamOwned<ClientConnection, TcpStream>;

/// Why the builders cannot fail: ring's provider has TLS 1.2 and 1.3.
const VERSIONS: &str = "ring's provider has the default versions";

fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring::default_provider())
}

/// The root certificates the system trusts, and those in the PEM files
/// `extra`; says what it could not read.
pub fn roots(extra: &[String], who: &str) -> RootCertStore {
    let mut store = RootCertStore::empty();
    let mut add = |path: &str| -> bool {
        let Ok(certificates) = CertificateDer::pem_file_iter(path) else { return false };
        let (added, _) = store.add_parsable_certificates(certificates.flatten());
        added > 0
    };
    if !ROOTS.iter().any(|path| add(path)) {
        eprintln!("{who}: no root certificates in {}", ROOTS[0]);
    }
    for path in extra {
        if !add(path) {
            eprintln!("{who}: no certificates in {path}");
        }
    }
    store
}

/// A client's settings that check the server's certificate against
/// `roots`, offering the protocols `alpn`.
pub fn verified(roots: RootCertStore, alpn: &[&[u8]]) -> Arc<ClientConfig> {
    let mut config = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect(VERSIONS)
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Arc::new(config)
}

/// A client's settings that take any certificate whose key signed the
/// handshake: for opportunistic DNS over TLS.
pub fn opportunistic(alpn: &[&[u8]]) -> Arc<ClientConfig> {
    let verifier = Arc::new(AnyCertificate(provider()));
    let mut config = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect(VERSIONS)
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Arc::new(config)
}

/// Connects to `address` and makes a TLS connection for `name` with
/// `config`, within `timeout` for each step.
pub fn connect(
    address: SocketAddr,
    name: ServerName<'static>,
    config: &Arc<ClientConfig>,
    timeout: Duration,
) -> io::Result<Stream> {
    let tcp = TcpStream::connect_timeout(&address, timeout)?;
    tcp.set_read_timeout(Some(timeout))?;
    tcp.set_write_timeout(Some(timeout))?;
    let _ = tcp.set_nodelay(true);
    let connection = ClientConnection::new(config.clone(), name).map_err(io::Error::other)?;
    let mut stream = StreamOwned::new(connection, tcp);
    while stream.conn.is_handshaking() {
        stream.conn.complete_io(&mut stream.sock)?;
    }
    Ok(stream)
}

/// The name to check a server's certificate for: `name`, or its address.
pub fn server_name(name: Option<&str>, address: SocketAddr) -> io::Result<ServerName<'static>> {
    match name {
        Some(name) => ServerName::try_from(name.to_string())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "not a server name")),
        None => Ok(ServerName::IpAddress(address.ip().into())),
    }
}

/// Writes all of `data`, then flushes it out.
pub fn send(stream: &mut Stream, data: &[u8]) -> io::Result<()> {
    stream.write_all(data)?;
    stream.flush()
}

/// Takes any certificate, but checks that its key signed the handshake.
#[derive(Debug)]
struct AnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
