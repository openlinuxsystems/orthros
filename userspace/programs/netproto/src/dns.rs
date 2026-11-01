// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! DNS messages (RFC 1035), as far as resolved(8) and resolvectl(1) look
//! into them (ADR 0193): the header, the one question, and each record's
//! type and time to live, to cache an answer for as long as it may be
//! kept (RFC 2308 for answers that a name or its data is not there) and
//! to age it when it is given out again. Names are compared without
//! regard to case, as DNS compares them. Everything is checked against
//! the message's length: a message that runs out, or a name whose
//! pointers loop, is not a message.

use std::net::{Ipv4Addr, Ipv6Addr};

/// The header's length.
pub const HEADER: usize = 12;

/// Header flags. A response, not a query.
pub const QR: u16 = 1 << 15;
/// Truncated: the answer did not fit.
pub const TC: u16 = 1 << 9;
/// Recursion desired.
pub const RD: u16 = 1 << 8;
/// Recursion available.
pub const RA: u16 = 1 << 7;
/// Authentic data: the server checked its DNSSEC signatures.
pub const AD: u16 = 1 << 5;
/// Checking disabled: the client checks DNSSEC itself.
pub const CD: u16 = 1 << 4;

/// Response codes. No error.
pub const NOERROR: u16 = 0;
/// The server failed.
pub const SERVFAIL: u16 = 2;
/// No such name.
pub const NXDOMAIN: u16 = 3;
/// The server refused.
pub const REFUSED: u16 = 5;

/// Record types. An IPv4 address.
pub const A: u16 = 1;
/// A name server.
pub const NS: u16 = 2;
/// The canonical name of an alias.
pub const CNAME: u16 = 5;
/// The start of a zone of authority.
pub const SOA: u16 = 6;
/// A pointer, to a name: what an address is looked up backwards for.
pub const PTR: u16 = 12;
/// A mail exchanger.
pub const MX: u16 = 15;
/// Text.
pub const TXT: u16 = 16;
/// An IPv6 address.
pub const AAAA: u16 = 28;
/// A service's server and port.
pub const SRV: u16 = 33;
/// EDNS(0)'s pseudo-record (RFC 6891).
pub const OPT: u16 = 41;
/// Every type, in a question.
pub const ANY: u16 = 255;
/// The Internet class.
pub const IN: u16 = 1;

/// The longest a cached answer is kept, as most resolvers cap it: a day.
pub const TTL_MAX: u32 = 86_400;
/// The longest an answer that a name or its data is not there is kept
/// (RFC 2308 suggests up to three hours).
pub const NEGATIVE_TTL_MAX: u32 = 3_600;

/// A message's header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// The query's number, which its answer repeats.
    pub id: u16,
    /// The flags, opcode and response code.
    pub flags: u16,
    /// How many questions, answers, authorities and additional records follow.
    pub questions: u16,
    /// How many answers.
    pub answers: u16,
    /// How many authority records.
    pub authorities: u16,
    /// How many additional records.
    pub additionals: u16,
}

impl Header {
    /// The response code.
    pub fn rcode(&self) -> u16 {
        self.flags & 0xf
    }
}

fn u16_at(message: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*message.get(at)?, *message.get(at + 1)?]))
}

fn u32_at(message: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(message.get(at..at + 4)?.try_into().ok()?))
}

/// The header of `message`.
pub fn header(message: &[u8]) -> Option<Header> {
    Some(Header {
        id: u16_at(message, 0)?,
        flags: u16_at(message, 2)?,
        questions: u16_at(message, 4)?,
        answers: u16_at(message, 6)?,
        authorities: u16_at(message, 8)?,
        additionals: u16_at(message, 10)?,
    })
}

/// Sets the message's number.
pub fn set_id(message: &mut [u8], id: u16) {
    if let Some(field) = message.get_mut(0..2) {
        field.copy_from_slice(&id.to_be_bytes());
    }
}

/// Reads the name at `at`: its labels, lower-cased and joined by dots
/// (the root is ""), and where what follows it starts. Pointers are
/// followed backwards only, so they cannot loop.
pub fn read_name(message: &[u8], at: usize) -> Option<(String, usize)> {
    let mut name = String::new();
    let mut pos = at;
    let mut end = None;
    let mut length = 0;
    loop {
        let len = *message.get(pos)?;
        match len >> 6 {
            0 if len == 0 => {
                return Some((name, end.unwrap_or(pos + 1)));
            }
            0 => {
                let label = message.get(pos + 1..pos + 1 + usize::from(len))?;
                length += label.len() + 1;
                if length > 255 {
                    return None;
                }
                if !name.is_empty() {
                    name.push('.');
                }
                for &byte in label {
                    match byte {
                        b'.' | b'\\' => {
                            name.push('\\');
                            name.push(char::from(byte));
                        }
                        0x21..=0x7e => name.push(char::from(byte.to_ascii_lowercase())),
                        _ => name.push_str(&format!("\\{byte:03}")),
                    }
                }
                pos += 1 + usize::from(len);
            }
            3 => {
                let target = usize::from(u16_at(message, pos)? & 0x3fff);
                if target >= pos {
                    return None;
                }
                end.get_or_insert(pos + 2);
                pos = target;
            }
            _ => return None,
        }
    }
}

/// Where the name at `at` ends.
fn skip_name(message: &[u8], at: usize) -> Option<usize> {
    let mut pos = at;
    loop {
        let len = *message.get(pos)?;
        match len >> 6 {
            0 if len == 0 => return Some(pos + 1),
            0 => pos += 1 + usize::from(len),
            3 => {
                message.get(pos + 1)?;
                return Some(pos + 2);
            }
            _ => return None,
        }
    }
}

/// A message's question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    /// The name, lower-cased.
    pub name: String,
    /// The type asked for.
    pub qtype: u16,
    /// The class, IN.
    pub qclass: u16,
    /// Where the question ends.
    pub end: usize,
}

/// The one question of `message`.
pub fn question(message: &[u8]) -> Option<Question> {
    if header(message)?.questions != 1 {
        return None;
    }
    let (name, at) = read_name(message, HEADER)?;
    Some(Question {
        name,
        qtype: u16_at(message, at)?,
        qclass: u16_at(message, at + 2)?,
        end: at + 4,
    })
}

/// A record of a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    /// 0 for the answers, 1 the authorities and 2 the additionals.
    pub section: u8,
    /// Its type.
    pub rtype: u16,
    /// Its class, or for OPT the UDP size.
    pub class: u16,
    /// How long, in seconds, it may be kept.
    pub ttl: u32,
    /// Where its owner's name starts.
    pub owner: usize,
    /// Where its time to live is.
    pub ttl_at: usize,
    /// Where its data starts and ends.
    pub data: (usize, usize),
}

/// Every record after the question, or `None` if the message does not
/// hold as many as its header says.
pub fn records(message: &[u8]) -> Option<Vec<Record>> {
    let header = header(message)?;
    let mut at = HEADER;
    for _ in 0..header.questions {
        at = skip_name(message, at)? + 4;
    }
    let mut records = Vec::new();
    for (section, count) in
        [header.answers, header.authorities, header.additionals].into_iter().enumerate()
    {
        for _ in 0..count {
            let owner = at;
            at = skip_name(message, at)?;
            let rtype = u16_at(message, at)?;
            let class = u16_at(message, at + 2)?;
            let ttl = u32_at(message, at + 4)?;
            let len = usize::from(u16_at(message, at + 8)?);
            let start = at + 10;
            message.get(start..start + len)?;
            records.push(Record {
                section: section as u8,
                rtype,
                class,
                ttl,
                owner,
                ttl_at: at + 4,
                data: (start, start + len),
            });
            at = start + len;
        }
    }
    Some(records)
}

/// How long the answer `message` may be kept, in seconds: the shortest
/// time to live of its records, or for one that says its name or data is
/// not there, its SOA's time to live or minimum, the shorter (RFC 2308);
/// `None` for one not to keep: a failure, a truncated one, or one with
/// nothing that says how long.
pub fn keep_for(message: &[u8]) -> Option<u32> {
    let header = header(message)?;
    if header.flags & TC != 0 || !matches!(header.rcode(), NOERROR | NXDOMAIN) {
        return None;
    }
    let records = records(message)?;
    let answers = records.iter().filter(|r| r.section == 0);
    if header.rcode() == NOERROR && header.answers > 0 {
        let others = records.iter().filter(|r| r.section > 0 && r.rtype != OPT);
        let ttl = answers.chain(others).map(|r| r.ttl).min()?;
        return Some(ttl.min(TTL_MAX));
    }
    // NXDOMAIN, or no data: the SOA among the authorities says how long.
    let soa = records.iter().find(|r| r.section == 1 && r.rtype == SOA)?;
    let (start, end) = soa.data;
    // The SOA's two names, then serial, refresh, retry, expire, minimum.
    let after = skip_name(message, skip_name(message, start)?)?;
    let minimum = u32_at(message, after + 16)?;
    if after + 20 != end {
        return None;
    }
    Some(soa.ttl.min(minimum).min(NEGATIVE_TTL_MAX))
}

/// Takes `seconds` from the time to live of each record of `message`
/// but its OPT, for an answer given out of the cache that long after it
/// came.
pub fn age(message: &mut [u8], seconds: u32) {
    let Some(records) = records(message) else { return };
    for record in records.iter().filter(|r| r.rtype != OPT) {
        let ttl = record.ttl.saturating_sub(seconds);
        message[record.ttl_at..record.ttl_at + 4].copy_from_slice(&ttl.to_be_bytes());
    }
}

/// Whether the query asks for DNSSEC records: its OPT's DO bit.
pub fn wants_dnssec(message: &[u8]) -> bool {
    records(message).is_some_and(|records| {
        records.iter().any(|r| r.section == 2 && r.rtype == OPT && r.ttl & 0x8000 != 0)
    })
}

/// The largest answer a client takes over UDP: its OPT's size, or 512
/// without one (RFC 6891).
pub fn udp_size(message: &[u8]) -> usize {
    records(message)
        .and_then(|records| {
            records.iter().find(|r| r.section == 2 && r.rtype == OPT).map(|r| usize::from(r.class))
        })
        .map_or(512, |size| size.max(512))
}

/// Lowers the UDP size `message`'s OPT record offers to `max`, if it
/// offers more: a forwarder asks no more of the servers it forwards to
/// than what arrives unfragmented.
pub fn limit_udp_size(message: &mut [u8], max: u16) {
    let Some(records) = records(message) else { return };
    if let Some(opt) = records.iter().find(|r| r.section == 2 && r.rtype == OPT && r.class > max) {
        message[opt.ttl_at - 2..opt.ttl_at].copy_from_slice(&max.to_be_bytes());
    }
}

/// The start of an answer to `query`: its header, as a response with
/// `rcode` and recursion available, and its question.
fn answer_start(query: &[u8], question: &Question, rcode: u16) -> Vec<u8> {
    let header = header(query).unwrap_or(Header {
        id: 0,
        flags: 0,
        questions: 0,
        answers: 0,
        authorities: 0,
        additionals: 0,
    });
    let flags = QR | (header.flags & (RD | CD | 0x7800)) | RA | rcode;
    let mut out = Vec::with_capacity(question.end + 32);
    out.extend_from_slice(&header.id.to_be_bytes());
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]);
    out.extend_from_slice(&query[HEADER..question.end]);
    out
}

/// A failure for `query`: SERVFAIL, REFUSED or the like, with its
/// question.
pub fn failure(query: &[u8], question: &Question, rcode: u16) -> Vec<u8> {
    answer_start(query, question, rcode)
}

/// `answer`, cut to its header and question with the TC bit set if it is
/// longer than `max`, as a UDP client is told to ask again over TCP.
pub fn fit(answer: Vec<u8>, max: usize) -> Vec<u8> {
    if answer.len() <= max {
        return answer;
    }
    let Some(question) = question(&answer) else { return answer };
    let mut cut = answer[..question.end].to_vec();
    let flags = u16_at(&cut, 2).unwrap_or(0) | TC;
    cut[2..4].copy_from_slice(&flags.to_be_bytes());
    cut[6..12].fill(0);
    cut
}

/// An answer the resolver gives itself, without asking a server: for
/// `localhost` and the names below it, and their addresses' reverse
/// names (RFC 6761), and NXDOMAIN for `invalid` and below, and for the
/// `.local` names that mDNS answers on the link (RFC 6762), never a DNS
/// server.
pub fn local_answer(query: &[u8], question: &Question) -> Option<Vec<u8>> {
    let name = question.name.as_str();
    let below = |zone: &str| name == zone || name.ends_with(&format!(".{zone}"));
    if below("invalid") || below("local") {
        return Some(answer_start(query, question, NXDOMAIN));
    }
    let localhost = below("localhost");
    let reverse = name == "1.0.0.127.in-addr.arpa"
        || name == "1.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.ip6.arpa";
    if !localhost && !reverse {
        return None;
    }
    let mut out = answer_start(query, question, NOERROR);
    let mut record = |rtype: u16, data: &[u8]| {
        // A pointer to the question's name.
        out.extend_from_slice(&[0xc0, HEADER as u8]);
        out.extend_from_slice(&rtype.to_be_bytes());
        out.extend_from_slice(&IN.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&(data.len() as u16).to_be_bytes());
        out.extend_from_slice(data);
    };
    let any = question.qtype == ANY;
    let mut count = 0u16;
    if question.qclass == IN || question.qclass == ANY {
        if localhost && (question.qtype == A || any) {
            record(A, &Ipv4Addr::LOCALHOST.octets());
            count += 1;
        }
        if localhost && (question.qtype == AAAA || any) {
            record(AAAA, &Ipv6Addr::LOCALHOST.octets());
            count += 1;
        }
        if reverse && (question.qtype == PTR || any) {
            record(PTR, b"\x09localhost\x00");
            count += 1;
        }
    }
    out[6..8].copy_from_slice(&count.to_be_bytes());
    Some(out)
}

/// A query for `name` of type `qtype`, numbered `id`, asking for
/// recursion, with an OPT record offering `udp` bytes (RFC 6891).
pub fn query(name: &str, qtype: u16, id: u16, udp: u16) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(name.len() + 32);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&RD.to_be_bytes());
    out.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 1]);
    let trimmed = name.strip_suffix('.').unwrap_or(name);
    if !trimmed.is_empty() {
        for label in trimmed.split('.') {
            let len = u8::try_from(label.len()).ok().filter(|&len| (1..64).contains(&len))?;
            out.push(len);
            out.extend_from_slice(label.as_bytes());
        }
    }
    out.push(0);
    if out.len() - HEADER > 255 {
        return None;
    }
    out.extend_from_slice(&qtype.to_be_bytes());
    out.extend_from_slice(&IN.to_be_bytes());
    // The OPT record: the root's name, its type, the size, no extended
    // flags, no options.
    out.push(0);
    out.extend_from_slice(&OPT.to_be_bytes());
    out.extend_from_slice(&udp.to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    Some(out)
}

/// Whether `answer` answers the question `asked`: the same name, type and
/// class, as the response it says it is.
pub fn answers(answer: &[u8], asked: &Question) -> bool {
    let Some(header) = header(answer) else { return false };
    header.flags & QR != 0
        && question(answer).is_some_and(|got| {
            got.name == asked.name && got.qtype == asked.qtype && got.qclass == asked.qclass
        })
}

/// A record's type as dig(1) names it.
pub fn type_name(rtype: u16) -> String {
    match rtype {
        A => "A".into(),
        NS => "NS".into(),
        CNAME => "CNAME".into(),
        SOA => "SOA".into(),
        PTR => "PTR".into(),
        MX => "MX".into(),
        TXT => "TXT".into(),
        AAAA => "AAAA".into(),
        SRV => "SRV".into(),
        OPT => "OPT".into(),
        ANY => "ANY".into(),
        other => format!("TYPE{other}"),
    }
}

/// The type dig(1) calls `name`.
pub fn type_by_name(name: &str) -> Option<u16> {
    let upper = name.to_ascii_uppercase();
    [A, NS, CNAME, SOA, PTR, MX, TXT, AAAA, SRV, ANY]
        .into_iter()
        .find(|&t| type_name(t) == upper)
        .or_else(|| upper.strip_prefix("TYPE").and_then(|n| n.parse().ok()))
}

/// The data of `record` of `message` as text, as dig(1) shows it, for the
/// types resolvectl shows; others as their length.
pub fn data_text(message: &[u8], record: &Record) -> String {
    let (start, end) = record.data;
    let data = &message[start..end];
    let name =
        |at| read_name(message, at).map_or_else(|| "?".into(), |(name, _)| format!("{name}."));
    match record.rtype {
        A if data.len() == 4 => Ipv4Addr::new(data[0], data[1], data[2], data[3]).to_string(),
        AAAA if data.len() == 16 => {
            Ipv6Addr::from(<[u8; 16]>::try_from(data).unwrap_or([0; 16])).to_string()
        }
        CNAME | NS | PTR => name(start),
        MX if data.len() > 2 => {
            format!("{} {}", u16_at(message, start).unwrap_or(0), name(start + 2))
        }
        TXT => {
            let mut parts = Vec::new();
            let mut at = 0;
            while let Some(&len) = data.get(at) {
                let text = data.get(at + 1..at + 1 + usize::from(len)).unwrap_or_default();
                parts.push(format!("\"{}\"", String::from_utf8_lossy(text)));
                at += 1 + usize::from(len);
            }
            parts.join(" ")
        }
        _ => format!("\\# {}", data.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An answer for www.example.com A: a CNAME to example.com (TTL 300)
    /// and its address (TTL 60), with names compressed.
    fn answer() -> Vec<u8> {
        let mut m = vec![0x12, 0x34, 0x81, 0x80, 0, 1, 0, 2, 0, 0, 0, 0];
        m.extend_from_slice(b"\x03WWW\x07example\x03com\x00\x00\x01\x00\x01");
        // www.example.com CNAME example.com
        m.extend_from_slice(&[0xc0, 12, 0, 5, 0, 1, 0, 0, 1, 44, 0, 2, 0xc0, 16]);
        // example.com A 93.184.216.34
        m.extend_from_slice(&[0xc0, 16, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 93, 184, 216, 34]);
        m
    }

    #[test]
    fn questions_and_records_read() {
        let m = answer();
        let q = question(&m).unwrap();
        assert_eq!((q.name.as_str(), q.qtype, q.qclass, q.end), ("www.example.com", A, IN, 33));
        let records = records(&m).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!((records[0].rtype, records[0].ttl), (CNAME, 300));
        assert_eq!(data_text(&m, &records[0]), "example.com.");
        assert_eq!(data_text(&m, &records[1]), "93.184.216.34");
        assert_eq!(keep_for(&m), Some(60));
    }

    #[test]
    fn answers_age_and_fit() {
        let mut m = answer();
        age(&mut m, 50);
        let records = records(&m).unwrap();
        assert_eq!((records[0].ttl, records[1].ttl), (250, 10));
        age(&mut m, 50);
        assert_eq!(records_ttls(&m), [200, 0]);
        let cut = fit(m.clone(), 40);
        assert_eq!(cut.len(), 33);
        let header = header(&cut).unwrap();
        assert!(header.flags & TC != 0);
        assert_eq!((header.questions, header.answers), (1, 0));
        assert_eq!(fit(m.clone(), 512), m);
    }

    fn records_ttls(m: &[u8]) -> Vec<u32> {
        records(m).unwrap().iter().map(|r| r.ttl).collect()
    }

    #[test]
    fn a_name_that_is_not_there_is_kept_as_its_soa_says() {
        let mut m = vec![0, 7, 0x81, 0x83, 0, 1, 0, 0, 0, 1, 0, 0];
        m.extend_from_slice(b"\x04nope\x07example\x00\x00\x01\x00\x01");
        // example SOA ns. host. 1 2 3 4 300, TTL 900
        m.extend_from_slice(&[0xc0, 17, 0, 6, 0, 1, 0, 0, 3, 132, 0, 30]);
        m.extend_from_slice(b"\x02ns\x00\x04host\x00");
        for value in [1u32, 2, 3, 4, 300] {
            m.extend_from_slice(&value.to_be_bytes());
        }
        assert_eq!(keep_for(&m), Some(300));
        // A failure is not kept.
        m[3] = 0x82;
        assert_eq!(keep_for(&m), None);
    }

    #[test]
    fn looping_and_short_messages_are_refused() {
        let mut m = vec![0, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        // A pointer to itself.
        m.extend_from_slice(&[0xc0, 12, 0, 1, 0, 1]);
        assert_eq!(question(&m), None);
        assert_eq!(question(&m[..5]), None);
        let mut a = answer();
        a.truncate(a.len() - 2);
        assert_eq!(records(&a), None);
        assert_eq!(keep_for(&a), None);
    }

    #[test]
    fn queries_are_made_and_answered_locally() {
        let q = query("LocalHost.", A, 9, 1232).unwrap();
        let asked = question(&q).unwrap();
        assert_eq!(asked.name, "localhost");
        assert_eq!(udp_size(&q), 1232);
        let mut big = query("example.com", A, 1, 4096).unwrap();
        limit_udp_size(&mut big, 1232);
        assert_eq!(udp_size(&big), 1232);
        limit_udp_size(&mut big, 4096);
        assert_eq!(udp_size(&big), 1232);
        assert!(!wants_dnssec(&q));
        let a = local_answer(&q, &asked).unwrap();
        assert!(answers(&a, &asked));
        let records = records(&a).unwrap();
        assert_eq!(data_text(&a, &records[0]), "127.0.0.1");
        assert_eq!(header(&a).unwrap().id, 9);
        let q = query("printer.local", AAAA, 1, 512).unwrap();
        let a = local_answer(&q, &question(&q).unwrap()).unwrap();
        assert_eq!(header(&a).unwrap().rcode(), NXDOMAIN);
        let q = query("example.com", A, 1, 512).unwrap();
        assert_eq!(local_answer(&q, &question(&q).unwrap()), None);
        assert_eq!(query(&"a".repeat(64), A, 1, 512), None);
        assert_eq!(type_by_name("aaaa"), Some(AAAA));
        assert_eq!(type_by_name("TYPE65"), Some(65));
    }
}
