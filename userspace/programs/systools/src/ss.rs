// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! ss(8): the TCP and UDP sockets, from `/processes/net/tcp` and `udp`
//! (radix-proc(5)), shown as iproute2's ss shows them (ADR 0190): the
//! connected ones, or with `-l` those that listen and the UDP ones that
//! are bound, or with `-a` all of them. Addresses are always numbers:
//! there is no name service to ask yet (Phase 12a.5).

use std::fs;
use std::io::{self, Write};
use std::net::Ipv4Addr;
use std::path::Path;

use crate::args::{Arg, Args};
use crate::util::{Fail, fail, usage};

/// Where the kernel lists the sockets.
const TCP: &str = "/processes/net/tcp";
const UDP: &str = "/processes/net/udp";

/// A socket, as the kernel lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Socket {
    tcp: bool,
    /// Linux's number for its state: 0x0a is listening, 0x07 closed (a
    /// UDP socket's, unless it is connected).
    state: u8,
    local: (Ipv4Addr, u16),
    remote: (Ipv4Addr, u16),
    send_queue: u32,
    receive_queue: u32,
}

const LISTEN: u8 = 0x0a;
const CLOSED: u8 = 0x07;
const SYN_RECEIVED: u8 = 0x03;
const TIME_WAIT: u8 = 0x06;

/// A state as ss(8) names it.
fn state_name(state: u8) -> &'static str {
    match state {
        0x01 => "ESTAB",
        0x02 => "SYN-SENT",
        SYN_RECEIVED => "SYN-RECV",
        0x04 => "FIN-WAIT-1",
        0x05 => "FIN-WAIT-2",
        TIME_WAIT => "TIME-WAIT",
        CLOSED => "UNCONN",
        0x08 => "CLOSE-WAIT",
        0x09 => "LAST-ACK",
        LISTEN => "LISTEN",
        0x0b => "CLOSING",
        _ => "UNKNOWN",
    }
}

/// `0F02000A:0050`: an address's bytes, in network order, read as a
/// little-endian number, and a port.
fn endpoint(text: &str) -> Option<(Ipv4Addr, u16)> {
    let (address, port) = text.split_once(':')?;
    let address = u32::from_str_radix(address, 16).ok()?;
    Some((Ipv4Addr::from(address.to_le_bytes()), u16::from_str_radix(port, 16).ok()?))
}

/// The sockets the file at `path` lists, of TCP if `tcp`.
fn read_sockets(path: &Path, tcp: bool) -> io::Result<Vec<Socket>> {
    let text = fs::read_to_string(path)?;
    Ok(text
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (send_queue, receive_queue) = fields.get(4)?.split_once(':')?;
            Some(Socket {
                tcp,
                local: endpoint(fields.get(1)?)?,
                remote: endpoint(fields.get(2)?)?,
                state: u8::from_str_radix(fields.get(3)?, 16).ok()?,
                send_queue: u32::from_str_radix(send_queue, 16).ok()?,
                receive_queue: u32::from_str_radix(receive_queue, 16).ok()?,
            })
        })
        .collect())
}

/// Which sockets ss(8) shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Which {
    /// The connected ones, as ss(8) shows by default: neither listening
    /// nor closed, nor ending in TIME-WAIT or half open in SYN-RECV.
    Connected,
    Listening,
    All,
}

impl Which {
    fn shows(self, socket: &Socket) -> bool {
        let listening = socket.state == LISTEN || socket.state == CLOSED;
        match self {
            Which::Connected => {
                !listening && socket.state != TIME_WAIT && socket.state != SYN_RECEIVED
            }
            Which::Listening => listening,
            Which::All => true,
        }
    }
}

/// `address:port`, or `address:*` for none.
fn place((address, port): (Ipv4Addr, u16)) -> (String, String) {
    (address.to_string(), if port == 0 { "*".to_string() } else { port.to_string() })
}

/// Writes `sockets` as ss(8) does: a column of each one's protocol when
/// there are both, its state, its queues and its two ends, each as wide
/// as its widest.
fn show(sockets: &[Socket], both: bool, header: bool, out: &mut dyn Write) -> io::Result<()> {
    let mut rows = Vec::new();
    if header {
        rows.push([
            "Netid".to_string(),
            "State".to_string(),
            "Recv-Q".to_string(),
            "Send-Q".to_string(),
            "Local Address".to_string(),
            "Port".to_string(),
            "Peer Address".to_string(),
            "Port".to_string(),
        ]);
    }
    for socket in sockets {
        let (local, local_port) = place(socket.local);
        let (remote, remote_port) = place(socket.remote);
        rows.push([
            if socket.tcp { "tcp" } else { "udp" }.to_string(),
            state_name(socket.state).to_string(),
            socket.receive_queue.to_string(),
            socket.send_queue.to_string(),
            local,
            local_port,
            remote,
            remote_port,
        ]);
    }
    let mut widths = [0; 8];
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.len());
        }
    }
    for row in &rows {
        let mut line = String::new();
        if both {
            line += &format!("{:<w$} ", row[0], w = widths[0]);
        }
        line += &format!(
            "{:<w1$} {:<w2$} {:<w3$} {:>w4$}:{:<w5$} {:>w6$}:{}",
            row[1],
            row[2],
            row[3],
            row[4],
            row[5],
            row[6],
            row[7],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3],
            w4 = widths[4],
            w5 = widths[5],
            w6 = widths[6],
        );
        writeln!(out, "{}", line.trim_end())?;
    }
    Ok(())
}

pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let (mut tcp, mut udp, mut header) = (false, false, true);
    let mut which = Which::Connected;
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('t') => tcp = true,
            Arg::Short('u') => udp = true,
            Arg::Short('l') => which = Which::Listening,
            Arg::Short('a') => which = Which::All,
            Arg::Short('H') => header = false,
            // Numbers are all it shows, and IPv4 all there is.
            Arg::Short('n' | '4') => {}
            Arg::Short('6') => return Err(usage("IPv6 comes with Phase 12c".into())),
            Arg::Long(name) => match name.as_str() {
                "tcp" => tcp = true,
                "udp" => udp = true,
                "listening" => which = Which::Listening,
                "all" => which = Which::All,
                "no-header" => header = false,
                "numeric" | "ipv4" => {}
                _ => return Err(args.unknown(&Arg::Long(name))),
            },
            other => return Err(args.unknown(&other)),
        }
    }
    if !tcp && !udp {
        (tcp, udp) = (true, true);
    }
    let mut sockets = Vec::new();
    for (wanted, path, is_tcp) in [(tcp, TCP, true), (udp, UDP, false)] {
        if wanted {
            let listed = read_sockets(Path::new(path), is_tcp)
                .map_err(|error| fail(format!("{path}: {error}")))?;
            sockets.extend(listed.into_iter().filter(|socket| which.shows(socket)));
        }
    }
    show(&sockets, tcp && udp, header, &mut io::stdout().lock())?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A listening socket, a connection to it, and a bound UDP socket,
    /// as the kernel lists them.
    const TCP_LIST: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 0
   1: 0F02000A:0016 0202000A:D431 01 00000024:00000000 00:00000000 00000000     0        0 0
   2: 0F02000A:C000 0202000A:0050 06 00000000:00000000 00:00000000 00000000     0        0 0
";
    const UDP_LIST: &str = "   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
    0: 00000000:0044 00000000:0000 07 00000000:00000200 00:00000000 00000000     0        0 0
";

    fn sockets() -> Vec<Socket> {
        // A directory for each call: the tests run at once.
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let call = CALLS.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ss-test-{}-{call}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("tcp"), TCP_LIST).unwrap();
        fs::write(dir.join("udp"), UDP_LIST).unwrap();
        let mut sockets = read_sockets(&dir.join("tcp"), true).unwrap();
        sockets.extend(read_sockets(&dir.join("udp"), false).unwrap());
        sockets
    }

    fn shown(which: Which, both: bool) -> String {
        let shown: Vec<Socket> = sockets().into_iter().filter(|s| which.shows(s)).collect();
        let mut out = Vec::new();
        show(&shown, both, true, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn sockets_are_read_as_the_kernel_lists_them() {
        let sockets = sockets();
        assert_eq!(sockets.len(), 4);
        assert_eq!(sockets[1].local, (Ipv4Addr::new(10, 0, 2, 15), 22));
        assert_eq!(sockets[1].remote, (Ipv4Addr::new(10, 0, 2, 2), 54321));
        assert_eq!((sockets[1].state, sockets[1].send_queue), (1, 36));
        assert_eq!((sockets[3].tcp, sockets[3].receive_queue), (false, 512));
    }

    #[test]
    fn connections_are_shown_by_default() {
        assert_eq!(
            shown(Which::Connected, false),
            "State Recv-Q Send-Q Local Address:Port Peer Address:Port\n\
             ESTAB 0      36         10.0.2.15:22       10.0.2.2:54321\n"
        );
    }

    #[test]
    fn listening_sockets_and_bound_ones_are_shown_with_l() {
        assert_eq!(
            shown(Which::Listening, true),
            "Netid State  Recv-Q Send-Q Local Address:Port Peer Address:Port\n\
             tcp   LISTEN 0      0            0.0.0.0:22        0.0.0.0:*\n\
             udp   UNCONN 512    0            0.0.0.0:68        0.0.0.0:*\n"
        );
        assert_eq!(shown(Which::All, true).lines().count(), 5);
        assert!(shown(Which::All, true).contains("TIME-WAIT"));
    }
}
