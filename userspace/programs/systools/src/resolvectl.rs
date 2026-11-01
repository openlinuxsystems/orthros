// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! resolvectl(1): what resolved(8) asks and how (ADR 0193), names looked
//! up through it, and its cache emptied, with systemd's resolvectl's
//! commands.
//!
//! `status` shows the file resolved writes; `query` asks resolved, over
//! UDP and again over TCP for an answer too long for it, and shows the
//! answer's records as dig(1) does; `flush-caches` sends resolved
//! SIGUSR2, which empties its cache.

use std::fs;
use std::io::{self, Read, Write};
use std::net::{IpAddr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use crate::args::{Arg, Args};
use crate::resolved::{LISTEN, PID, STATUS};
use crate::util::{self, Fail, fail, usage};
use netproto::dns;

/// How long resolved has to answer.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Runs resolvectl.
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut qtype = None;
    let mut operands = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('t') => {
                let name = args.text()?;
                qtype = Some(
                    dns::type_by_name(&name)
                        .ok_or_else(|| usage(format!("no record type '{name}'")))?,
                );
            }
            Arg::Long(long) if long == "type" => {
                let name = args.text()?;
                qtype = Some(
                    dns::type_by_name(&name)
                        .ok_or_else(|| usage(format!("no record type '{name}'")))?,
                );
            }
            Arg::Operand(word) => operands.push(word.to_string_lossy().into_owned()),
            other => return Err(args.unknown(&other)),
        }
    }
    let mut operands = operands.into_iter();
    match operands.next().as_deref() {
        None | Some("status") => {
            let text = fs::read_to_string(STATUS)
                .map_err(|e| util::failed(&format!("resolvectl: {STATUS}"), e))?;
            print!("{text}");
            Ok(0)
        }
        Some("query") => {
            let names: Vec<String> = operands.collect();
            if names.is_empty() {
                return Err(usage("resolvectl: query needs a name or an address".into()));
            }
            let mut code = 0;
            for name in &names {
                if let Err(why) = query(name, qtype) {
                    eprintln!("resolvectl: {name}: {why}");
                    code = 1;
                }
            }
            Ok(code)
        }
        Some("flush-caches") => {
            let pid = fs::read_to_string(PID)
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok())
                .ok_or_else(|| fail("resolvectl: resolved is not running".into()))?;
            // SAFETY: kill(2) takes numbers and touches no memory.
            if unsafe { libc::kill(pid, libc::SIGUSR2) } != 0 {
                return Err(util::failed(&"resolvectl: resolved", io::Error::last_os_error()));
            }
            Ok(0)
        }
        Some(other) => {
            Err(usage(format!("resolvectl: no command '{other}': status, query or flush-caches")))
        }
    }
}

/// The name a reverse lookup asks for `address`.
fn reverse_name(address: IpAddr) -> String {
    match address {
        IpAddr::V4(v4) => {
            let [a, b, c, d] = v4.octets();
            format!("{d}.{c}.{b}.{a}.in-addr.arpa")
        }
        IpAddr::V6(v6) => {
            let mut name = String::new();
            for byte in v6.octets().iter().rev() {
                name += &format!("{:x}.{:x}.", byte & 0xf, byte >> 4);
            }
            name + "ip6.arpa"
        }
    }
}

/// Looks `name` up through resolved and shows what came: its addresses
/// (A and AAAA) unless `qtype` says what to ask for; an address is
/// looked up backwards, for its PTR.
fn query(name: &str, qtype: Option<u16>) -> Result<(), String> {
    let (name, types) = match name.parse::<IpAddr>() {
        Ok(address) => (reverse_name(address), vec![qtype.unwrap_or(dns::PTR)]),
        Err(_) => (name.to_string(), qtype.map_or_else(|| vec![dns::A, dns::AAAA], |t| vec![t])),
    };
    let started = Instant::now();
    let mut shown = 0;
    let mut nothing = None;
    for qtype in types {
        let id = (std::process::id() as u16) ^ qtype;
        let message =
            dns::query(&name, qtype, id, 4096).ok_or_else(|| "not a name DNS takes".to_string())?;
        let answer = ask(&message).map_err(|e| util::strip_os_error(&e))?;
        let header = dns::header(&answer).ok_or("resolved's answer is not a DNS message")?;
        match header.rcode() {
            dns::NOERROR => {}
            dns::NXDOMAIN => {
                nothing = Some("no such name (NXDOMAIN)");
                continue;
            }
            dns::SERVFAIL => {
                nothing = Some("no server answered (SERVFAIL)");
                continue;
            }
            other => {
                nothing = Some(if other == dns::REFUSED { "refused" } else { "the server failed" });
                continue;
            }
        }
        let records = dns::records(&answer).ok_or("resolved's answer is cut short")?;
        for record in records.iter().filter(|r| r.section == 0) {
            let owner = dns::read_name(&answer, record.owner)
                .map_or_else(|| name.clone(), |(owner, _)| owner);
            println!(
                "{owner}.\t{}\tIN\t{}\t{}",
                record.ttl,
                dns::type_name(record.rtype),
                dns::data_text(&answer, record)
            );
            shown += 1;
        }
    }
    if shown == 0 {
        return Err(nothing.unwrap_or("no records of that type").to_string());
    }
    let ms = started.elapsed().as_millis();
    println!("-- {shown} record{} in {ms} ms", if shown == 1 { "" } else { "s" });
    Ok(())
}

/// Sends `message` to resolved and returns its answer: over UDP, and over
/// TCP if it was cut.
fn ask(message: &[u8]) -> io::Result<Vec<u8>> {
    let socket = UdpSocket::bind("127.0.0.1:0")?;
    socket.connect(LISTEN)?;
    socket.set_read_timeout(Some(TIMEOUT))?;
    socket.send(message)?;
    let mut buf = vec![0; 65_535];
    let id = dns::header(message).map_or(0, |h| h.id);
    let answer = loop {
        let len = socket.recv(&mut buf)?;
        if dns::header(&buf[..len]).is_some_and(|h| h.id == id) {
            break buf[..len].to_vec();
        }
    };
    if dns::header(&answer).is_none_or(|h| h.flags & dns::TC == 0) {
        return Ok(answer);
    }
    let mut stream = TcpStream::connect_timeout(&LISTEN.into(), TIMEOUT)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    let mut framed = (message.len() as u16).to_be_bytes().to_vec();
    framed.extend_from_slice(message);
    stream.write_all(&framed)?;
    let mut len = [0; 2];
    stream.read_exact(&mut len)?;
    let mut answer = vec![0; usize::from(u16::from_be_bytes(len))];
    stream.read_exact(&mut answer)?;
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_looked_up_backwards() {
        assert_eq!(reverse_name("10.0.2.3".parse().unwrap()), "3.2.0.10.in-addr.arpa");
        let v6 = reverse_name("::1".parse().unwrap());
        assert!(v6.starts_with("1.0.0.0."));
        assert!(v6.ends_with(".0.ip6.arpa"));
        assert_eq!(v6.len(), 64 + 8);
    }
}
