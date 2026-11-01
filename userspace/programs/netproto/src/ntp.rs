// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! NTP's packets (RFC 5905), as timed(8) sends and reads them, and
//! Network Time Security for them (RFC 8915, ADR 0194): the records of
//! NTS key establishment, the keys the TLS session exports, and the
//! extension fields that carry cookies and authenticate a packet with
//! AEAD_AES_SIV_CMAC_256 (RFC 5297). The test servers of the
//! network-services boot test build on the same pieces, for the other
//! end.

use aes_siv::Aes128SivAead;
use aes_siv::aead::{Aead, KeyInit, Payload};

/// The NTP port.
pub const NTP_PORT: u16 = 123;
/// NTS key establishment's port.
pub const NTS_KE_PORT: u16 = 4460;
/// The protocol NTS-KE's TLS connections offer (ALPN).
pub const NTS_ALPN: &[u8] = b"ntske/1";
/// The label NTS's keys are exported from the TLS session under.
pub const EXPORTER_LABEL: &[u8] = b"EXPORTER-network-time-security";

/// NTS-KE record types (RFC 8915 section 4). The end of a message.
pub const END_OF_MESSAGE: u16 = 0;
/// The protocols NTS is for: NTPv4.
pub const NEXT_PROTOCOL: u16 = 1;
/// The server's refusal, with its error code.
pub const ERROR: u16 = 2;
/// A warning from the server.
pub const WARNING: u16 = 3;
/// The AEAD algorithms offered, or the one chosen.
pub const AEAD_ALGORITHM: u16 = 4;
/// A cookie for one NTP request.
pub const NEW_COOKIE: u16 = 5;
/// The NTP server to ask, if not this one.
pub const SERVER: u16 = 6;
/// The NTP port to ask, if not 123.
pub const PORT: u16 = 7;
/// A record's critical bit.
pub const CRITICAL: u16 = 0x8000;
/// NTPv4, as NTS-KE numbers it.
pub const PROTOCOL_NTPV4: u16 = 0;
/// AEAD_AES_SIV_CMAC_256, the algorithm every NTS server has.
pub const AES_SIV_CMAC_256: u16 = 15;
/// Its key's length.
pub const KEY_LEN: usize = 32;

/// NTP extension field types for NTS (RFC 8915 section 5). A request's
/// random number, which its answer repeats.
pub const UNIQUE_IDENTIFIER: u16 = 0x0104;
/// A cookie, which the server reads its keys back from.
pub const COOKIE: u16 = 0x0204;
/// Room for one more cookie in the answer.
pub const COOKIE_PLACEHOLDER: u16 = 0x0304;
/// The authenticator and the fields it encrypts.
pub const AUTHENTICATOR: u16 = 0x0404;

/// The length of an NTP header.
pub const HEADER: usize = 48;
/// Seconds from 1900, NTP's era, to 1970.
pub const NTP_TO_UNIX: u64 = 2_208_988_800;

/// An NTP timestamp: seconds since 1900 and a fraction of 2^-32 s.
pub fn timestamp(unix_ns: u64) -> u64 {
    let seconds = unix_ns / 1_000_000_000 + NTP_TO_UNIX;
    let fraction = ((unix_ns % 1_000_000_000) << 32) / 1_000_000_000;
    (seconds << 32) | fraction
}

/// The Unix time, in nanoseconds, of the NTP timestamp `stamp` (in era
/// 0, to 2036, or era 1 after it, whichever is nearest `near_ns`).
pub fn unix_ns(stamp: u64, near_ns: u64) -> i128 {
    let seconds = i128::from(stamp >> 32);
    let fraction = (i128::from(stamp & 0xffff_ffff) * 1_000_000_000) >> 32;
    let mut ns = (seconds - i128::from(NTP_TO_UNIX)) * 1_000_000_000 + fraction;
    let era = (1i128 << 32) * 1_000_000_000;
    let near = i128::from(near_ns);
    while ns + era / 2 < near {
        ns += era;
    }
    ns
}

/// What an NTP server's answer says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Answer {
    /// The leap indicator: 3 for a server whose clock is not set.
    pub leap: u8,
    /// The NTP version.
    pub version: u8,
    /// 4 for a server's answer.
    pub mode: u8,
    /// How far the server is from a reference clock: 0 for a kiss of
    /// death, 16 for none.
    pub stratum: u8,
    /// Its reference ID: for stratum 0, a kiss code such as `NTSN`.
    pub reference: [u8; 4],
    /// The round trip to its reference clock, in NTP's short format.
    pub root_delay: u32,
    /// How far off it may be from its reference clock.
    pub root_dispersion: u32,
    /// The client's transmit timestamp, as the server copied it.
    pub origin: u64,
    /// When the server received the request.
    pub receive: u64,
    /// When the server sent its answer.
    pub transmit: u64,
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*data.get(at)?, *data.get(at + 1)?]))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

/// Reads an NTP packet's header.
pub fn answer(packet: &[u8]) -> Option<Answer> {
    let first = *packet.first()?;
    packet.get(..HEADER)?;
    Some(Answer {
        leap: first >> 6,
        version: (first >> 3) & 7,
        mode: first & 7,
        stratum: packet[1],
        reference: packet[12..16].try_into().ok()?,
        root_delay: u32_at(packet, 4)?,
        root_dispersion: u32_at(packet, 8)?,
        origin: u64_at(packet, 24)?,
        receive: u64_at(packet, 32)?,
        transmit: u64_at(packet, 40)?,
    })
}

/// A client's NTPv4 request, whose transmit timestamp is `transmit`.
pub fn request(transmit: u64) -> Vec<u8> {
    let mut packet = vec![0; HEADER];
    // No leap warning, version 4, mode 3 (client).
    packet[0] = 0x23;
    packet[40..48].copy_from_slice(&transmit.to_be_bytes());
    packet
}

/// A server's answer to `request`, as the test servers make it: stratum
/// `stratum`, received at `receive`, sent at `transmit`, all NTP
/// timestamps; or, for stratum 0, the kiss code `kiss`.
pub fn server_answer(request: &[u8], stratum: u8, receive: u64, transmit: u64, kiss: [u8; 4]) -> Vec<u8> {
    let mut packet = vec![0; HEADER];
    // Version as asked, mode 4 (server).
    let version = request.first().map_or(4, |first| (first >> 3) & 7);
    packet[0] = (version << 3) | 4;
    packet[1] = stratum;
    // Polls of 2^6 s, a precision of 2^-20 s.
    packet[2] = 6;
    packet[3] = 0xec;
    packet[12..16].copy_from_slice(if stratum == 0 { &kiss } else { b"DKVO" });
    packet[16..24].copy_from_slice(&receive.to_be_bytes());
    if let Some(origin) = request.get(40..48) {
        packet[24..32].copy_from_slice(origin);
    }
    packet[32..40].copy_from_slice(&receive.to_be_bytes());
    packet[40..48].copy_from_slice(&transmit.to_be_bytes());
    packet
}

/// An NTS-KE record.
pub fn record(kind: u16, critical: bool, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(kind | if critical { CRITICAL } else { 0 }).to_be_bytes());
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// What a client sends in NTS-KE: NTPv4, AES-SIV-CMAC-256, and the end.
pub fn ke_request() -> Vec<u8> {
    let mut out = record(NEXT_PROTOCOL, true, &PROTOCOL_NTPV4.to_be_bytes());
    out.extend(record(AEAD_ALGORITHM, true, &AES_SIV_CMAC_256.to_be_bytes()));
    out.extend(record(END_OF_MESSAGE, true, &[]));
    out
}

/// An NTS-KE record: its type without the critical bit, whether it is
/// critical, and its body.
pub type KeRecord = (u16, bool, Vec<u8>);

/// The records of an NTS-KE message to its End of Message, and where it
/// ends; `None` if the data ends first.
pub fn ke_records(data: &[u8]) -> Option<(Vec<KeRecord>, usize)> {
    let mut records = Vec::new();
    let mut at = 0;
    loop {
        let kind = u16_at(data, at)?;
        let len = usize::from(u16_at(data, at + 2)?);
        let body = data.get(at + 4..at + 4 + len)?.to_vec();
        at += 4 + len;
        let (kind, critical) = (kind & !CRITICAL, kind & CRITICAL != 0);
        if kind == END_OF_MESSAGE {
            return Some((records, at));
        }
        records.push((kind, critical, body));
    }
}

/// What an NTS-KE server answered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Established {
    /// The cookies, one for each NTP request.
    pub cookies: Vec<Vec<u8>>,
    /// The NTP server to ask, if not the NTS-KE server.
    pub server: Option<String>,
    /// Its port, if not 123.
    pub port: Option<u16>,
}

/// Reads an NTS-KE server's answer: its cookies and where to send NTP,
/// or why it is no good.
pub fn ke_answer(records: &[KeRecord]) -> Result<Established, String> {
    let mut established = Established::default();
    let (mut protocol, mut aead) = (false, false);
    for (kind, critical, body) in records {
        match *kind {
            NEXT_PROTOCOL => protocol = body.as_slice() == PROTOCOL_NTPV4.to_be_bytes(),
            AEAD_ALGORITHM => aead = body.as_slice() == AES_SIV_CMAC_256.to_be_bytes(),
            NEW_COOKIE if !body.is_empty() => established.cookies.push(body.clone()),
            SERVER => established.server = Some(String::from_utf8_lossy(body).into_owned()),
            PORT => established.port = u16_at(body, 0),
            ERROR => {
                return Err(format!("the server refused, with error {}", u16_at(body, 0).unwrap_or(0)));
            }
            WARNING => {}
            other if *critical => return Err(format!("a critical record of type {other} it does not know")),
            _ => {}
        }
    }
    if !protocol {
        return Err("the server offers no NTPv4".into());
    }
    if !aead {
        return Err("the server offers no AEAD_AES_SIV_CMAC_256".into());
    }
    if established.cookies.is_empty() {
        return Err("the server gave no cookies".into());
    }
    Ok(established)
}

/// The context NTS's keys are exported with: NTPv4, AES-SIV-CMAC-256,
/// and 0 for the client's key, 1 for the server's.
pub fn exporter_context(server_to_client: bool) -> [u8; 5] {
    let mut context = [0; 5];
    context[0..2].copy_from_slice(&PROTOCOL_NTPV4.to_be_bytes());
    context[2..4].copy_from_slice(&AES_SIV_CMAC_256.to_be_bytes());
    context[4] = u8::from(server_to_client);
    context
}

/// An NTP extension field, padded to a multiple of 4 bytes and at least
/// 16 (RFC 7822).
pub fn field(kind: u16, body: &[u8]) -> Vec<u8> {
    let len = (4 + body.len()).next_multiple_of(4).max(16);
    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(&kind.to_be_bytes());
    out.extend_from_slice(&(len as u16).to_be_bytes());
    out.extend_from_slice(body);
    out.resize(len, 0);
    out
}

/// The extension fields after a packet's header, as (type, where its
/// body starts, its length without padding is unknown: to the field's
/// end); `None` if they run past the packet.
pub fn fields(packet: &[u8]) -> Option<Vec<(u16, usize, usize)>> {
    let mut out = Vec::new();
    let mut at = HEADER;
    while at < packet.len() {
        let kind = u16_at(packet, at)?;
        let len = usize::from(u16_at(packet, at + 2)?);
        if len < 4 || len % 4 != 0 || at + len > packet.len() {
            return None;
        }
        out.push((kind, at, at + len));
        at += len;
    }
    Some(out)
}

/// Seals `plaintext` with `key`, the packet so far as associated data,
/// under `nonce`, into an NTS Authenticator and Encrypted Extension
/// Fields field.
pub fn authenticator(key: &[u8; KEY_LEN], nonce: &[u8; 16], packet: &[u8], plaintext: &[u8]) -> Option<Vec<u8>> {
    let cipher = Aes128SivAead::new(key.into());
    let sealed = cipher.encrypt(nonce.into(), Payload { msg: plaintext, aad: packet }).ok()?;
    let mut body = Vec::with_capacity(4 + 16 + sealed.len() + 3);
    body.extend_from_slice(&16u16.to_be_bytes());
    body.extend_from_slice(&(sealed.len() as u16).to_be_bytes());
    body.extend_from_slice(nonce);
    body.extend_from_slice(&sealed);
    Some(field(AUTHENTICATOR, &body))
}

/// Opens the Authenticator field that starts at `at` in `packet`, with
/// `key`: the encrypted extension fields it carries, if the packet up to
/// it is what was sealed.
pub fn open(key: &[u8; KEY_LEN], packet: &[u8], at: usize, end: usize) -> Option<Vec<u8>> {
    let body = packet.get(at + 4..end)?;
    let nonce_len = usize::from(u16_at(body, 0)?);
    let sealed_len = usize::from(u16_at(body, 2)?);
    let nonce = body.get(4..4 + nonce_len)?;
    let sealed_at = 4 + nonce_len.next_multiple_of(4);
    let sealed = body.get(sealed_at..sealed_at + sealed_len)?;
    if nonce.len() != 16 {
        return None;
    }
    let cipher = Aes128SivAead::new(key.into());
    cipher.decrypt(nonce.into(), Payload { msg: sealed, aad: &packet[..at] }).ok()
}

/// The bodies of the fields of type `kind` in `data`, a run of
/// extension fields from `start` (the plaintext an Authenticator carried
/// starts at 0).
pub fn bodies_of(data: &[u8], start: usize, kind: u16) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = start;
    while let (Some(k), Some(len)) = (u16_at(data, at), u16_at(data, at + 2)) {
        let len = usize::from(len);
        if len < 4 || at + len > data.len() {
            break;
        }
        if k == kind {
            out.push(data[at + 4..at + len].to_vec());
        }
        at += len;
    }
    out
}

/// A client's NTS-protected request: the header with transmit timestamp
/// `transmit`, a Unique Identifier, `cookie`, a placeholder for each of
/// `placeholders` more cookies wanted, and an Authenticator sealed with
/// the client's key `c2s` under `nonce`.
pub fn nts_request(
    transmit: u64,
    unique: &[u8; 32],
    cookie: &[u8],
    placeholders: usize,
    c2s: &[u8; KEY_LEN],
    nonce: &[u8; 16],
) -> Option<Vec<u8>> {
    let mut packet = request(transmit);
    packet.extend(field(UNIQUE_IDENTIFIER, unique));
    packet.extend(field(COOKIE, cookie));
    for _ in 0..placeholders {
        packet.extend(field(COOKIE_PLACEHOLDER, &vec![0; cookie.len()]));
    }
    let sealed = authenticator(c2s, nonce, &packet, &[])?;
    packet.extend(sealed);
    Some(packet)
}

/// Checks an NTS-protected answer to a request with `unique`, with the
/// server's key `s2c`: the new cookies it carries, if it is authentic.
pub fn nts_answer(packet: &[u8], unique: &[u8; 32], s2c: &[u8; KEY_LEN]) -> Result<Vec<Vec<u8>>, &'static str> {
    let fields = fields(packet).ok_or("its extension fields run past it")?;
    let echoed = fields
        .iter()
        .find(|(kind, _, _)| *kind == UNIQUE_IDENTIFIER)
        .is_some_and(|&(_, at, end)| packet[at + 4..end].starts_with(unique));
    if !echoed {
        return Err("it does not answer this request");
    }
    let &(_, at, end) = fields
        .iter()
        .find(|(kind, _, _)| *kind == AUTHENTICATOR)
        .ok_or("it is not authenticated")?;
    let plaintext = open(s2c, packet, at, end).ok_or("its authentication failed")?;
    Ok(bodies_of(&plaintext, 0, COOKIE))
}

/// The offset of the local clock from the server's, and the round trip's
/// delay, in nanoseconds, from the four timestamps: sent `t1`, received
/// by the server `t2`, sent back `t3`, received `t4` (RFC 5905).
pub fn offset_delay(t1: i128, t2: i128, t3: i128, t4: i128) -> (i128, i128) {
    (((t2 - t1) + (t3 - t4)) / 2, (t4 - t1) - (t3 - t2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_go_both_ways() {
        let ns = 1_761_955_200_123_456_789;
        let stamp = timestamp(ns);
        let back = unix_ns(stamp, ns);
        assert!((back - i128::from(ns)).abs() < 2);
        // After 2036, era 1.
        let late = 2_200_000_000_000_000_000u64;
        assert!((unix_ns(timestamp(late), late) - i128::from(late)).abs() < 2);
        assert_eq!(offset_delay(100, 150, 160, 120), (45, 10));
    }

    #[test]
    fn nts_ke_records_read() {
        let request = ke_request();
        let (records, end) = ke_records(&request).unwrap();
        assert_eq!(end, request.len());
        assert_eq!(records.len(), 2);
        let mut answer = record(NEXT_PROTOCOL, true, &[0, 0]);
        answer.extend(record(AEAD_ALGORITHM, false, &[0, 15]));
        answer.extend(record(NEW_COOKIE, false, b"cookie one"));
        answer.extend(record(NEW_COOKIE, false, b"cookie two"));
        answer.extend(record(PORT, true, &[0x01, 0x23]));
        answer.extend(record(END_OF_MESSAGE, true, &[]));
        let (records, _) = ke_records(&answer).unwrap();
        let established = ke_answer(&records).unwrap();
        assert_eq!(established.cookies.len(), 2);
        assert_eq!(established.port, Some(0x123));
        assert!(ke_records(&answer[..answer.len() - 1]).is_none());
        let unknown = record(99, true, &[]);
        assert!(ke_answer(&ke_records(&[unknown, record(0, true, &[])].concat()).unwrap().0).is_err());
    }

    #[test]
    fn nts_packets_seal_and_open() {
        let (c2s, s2c) = ([1; KEY_LEN], [2; KEY_LEN]);
        let unique = [7; 32];
        let request = nts_request(42, &unique, b"a cookie", 2, &c2s, &[3; 16]).unwrap();
        let fields = fields(&request).unwrap();
        let kinds: Vec<u16> = fields.iter().map(|f| f.0).collect();
        assert_eq!(kinds, [UNIQUE_IDENTIFIER, COOKIE, COOKIE_PLACEHOLDER, COOKIE_PLACEHOLDER, AUTHENTICATOR]);
        let &(_, at, end) = fields.last().unwrap();
        assert_eq!(open(&c2s, &request, at, end), Some(Vec::new()));
        assert_eq!(open(&s2c, &request, at, end), None);
        // The server's answer: its header, the identifier, and new
        // cookies sealed in its authenticator.
        let mut answer = server_answer(&request, 2, 5, 6, [0; 4]);
        answer.extend(field(UNIQUE_IDENTIFIER, &unique));
        let cookies = [field(COOKIE, b"new one"), field(COOKIE, b"new two")].concat();
        let sealed = authenticator(&s2c, &[4; 16], &answer, &cookies).unwrap();
        answer.extend(sealed);
        let got = nts_answer(&answer, &unique, &s2c).unwrap();
        assert_eq!(got.len(), 2);
        assert!(got[0].starts_with(b"new one"));
        // Changed after it was sealed, it fails.
        let mut forged = answer.clone();
        forged[41] ^= 1;
        assert!(nts_answer(&forged, &unique, &s2c).is_err());
        assert!(nts_answer(&answer, &[8; 32], &s2c).is_err());
        let header = super::answer(&answer).unwrap();
        assert_eq!((header.mode, header.stratum, header.origin, header.transmit), (4, 2, 42, 6));
    }
}
