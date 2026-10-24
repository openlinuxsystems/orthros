// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! shutdown(8), reboot(8), poweroff(8), halt(8) and service(8): requests
//! to Spark over its control socket, `/run/spark/control` (spark(8)).
//!
//! Spark stops the services in order and then turns the machine off,
//! restarts it or halts it; with `--force` it skips the services. The
//! options are systemd's and sysvinit's, and service(8) is Debian's.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;

use crate::args::{Arg, Args};
use crate::util::{self, Fail, fail, fail_with, usage};

/// Spark's control socket.
pub const CONTROL: &str = "/run/spark/control";

/// Sends Spark `request` and returns its answer.
pub fn request(request: &str) -> Result<String, Fail> {
    let mut stream = UnixStream::connect(CONTROL).map_err(|e| util::failed(&CONTROL, e))?;
    stream.write_all(format!("{request}\n").as_bytes())?;
    let _ = stream.shutdown(Shutdown::Write);
    let mut answer = String::new();
    stream.read_to_string(&mut answer)?;
    Ok(answer)
}

/// Sends `request`, and fails with what Spark said if it refused.
fn ask(request: &str) -> Result<String, Fail> {
    let answer = self::request(request)?;
    match answer.strip_prefix("error: ") {
        Some(why) => Err(fail(why.trim_end().to_string())),
        None => Ok(answer),
    }
}

/// How the machine ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    PowerOff,
    Reboot,
    Halt,
}

impl End {
    fn verb(self) -> &'static str {
        match self {
            End::PowerOff => "poweroff",
            End::Reboot => "reboot",
            End::Halt => "halt",
        }
    }
}

/// The seconds from `now` (seconds since the epoch, with `local_hm` its
/// local hour and minute) until `time`: `now`, `+MINUTES` or `HH:MM`, the
/// next time the clock says so.
fn delay(time: &str, local_hm: (u32, u32)) -> Option<u64> {
    if time == "now" {
        return Some(0);
    }
    if let Some(minutes) = time.strip_prefix('+') {
        return minutes.parse::<u64>().ok().map(|m| m * 60);
    }
    let (h, m) = time.split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    if h > 23 || m > 59 {
        return None;
    }
    let (now_h, now_m) = local_hm;
    let (at, now) = (h * 60 + m, now_h * 60 + now_m);
    let minutes = if at > now { at - now } else { at + 24 * 60 - now };
    Some(u64::from(minutes) * 60)
}

/// The local hour and minute now.
fn local_hm() -> (u32, u32) {
    let now = (util::now_ns() / 1_000_000_000) as i64;
    let hm = util::local_time(now, "%H %M");
    let mut parts = hm.split(' ').map(|p| p.parse().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

/// shutdown(8): `shutdown [-H|-P|-r|-h] [-k] [--no-wall] [TIME] [MESSAGE...]`, or
/// `shutdown -c`.
pub fn shutdown(args: &mut Args) -> Result<i32, Fail> {
    let mut end = End::PowerOff;
    let (mut cancel, mut warn_only, mut wall) = (false, false, true);
    let mut operands = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('H') => end = End::Halt,
            Arg::Long(l) if l == "halt" => end = End::Halt,
            Arg::Short('P' | 'h') => end = End::PowerOff,
            Arg::Long(l) if l == "poweroff" => end = End::PowerOff,
            Arg::Short('r') => end = End::Reboot,
            Arg::Long(l) if l == "reboot" => end = End::Reboot,
            Arg::Short('k') => warn_only = true,
            Arg::Long(l) if l == "no-wall" => wall = false,
            Arg::Short('c') => cancel = true,
            Arg::Long(l) if l == "help" => {
                println!(
                    "shutdown [OPTIONS...] [TIME] [WALL...]\n\n\
                     Shut down the system.\n\n\
                     \x20 -H --halt      Halt the machine\n\
                     \x20 -P --poweroff  Power off the machine (the default)\n\
                     \x20 -r --reboot    Reboot the machine\n\
                     \x20 -h             Equivalent to --poweroff\n\
                     \x20 -k             Don't halt/power off/reboot, just send warnings\n\
                     \x20    --no-wall   Don't send a wall message\n\
                     \x20 -c             Cancel a pending shutdown\n\n\
                     TIME is `now', `+MINUTES' or `HH:MM'; the default is +1."
                );
                return Ok(0);
            }
            Arg::Operand(value) => operands.push(value.to_string_lossy().into_owned()),
            other => return Err(args.unknown(&other)),
        }
    }
    if cancel {
        ask("cancel")?;
        if wall {
            eprintln!("The system shutdown has been cancelled.");
        }
        return Ok(0);
    }
    let time = operands.first().cloned().unwrap_or_else(|| "+1".into());
    let Some(seconds) = delay(&time, local_hm()) else {
        return Err(usage(format!("Failed to parse time specification: {time}")));
    };
    let message = operands.get(1..).map(|words| words.join(" ")).unwrap_or_default();
    let what = match end {
        End::PowerOff => "power-off",
        End::Reboot => "reboot",
        End::Halt => "halt",
    };
    if wall && !message.is_empty() {
        eprintln!("{message}");
    }
    if warn_only {
        eprintln!("The system will not {}: -k only warns.", end.verb());
        return Ok(0);
    }
    ask(&format!("{} {seconds}", end.verb()))?;
    if seconds > 0 {
        let at = (util::now_ns() / 1_000_000_000) as i64 + seconds as i64;
        let when = util::local_time(at, "%a %Y-%m-%d %H:%M:%S %Z");
        eprintln!("The system {what} is scheduled for {when}, use 'shutdown -c' to cancel.");
    }
    Ok(0)
}

/// reboot(8), poweroff(8) and halt(8): `NAME [-f] [-p] [--reboot|--poweroff|--halt]`.
fn end_now(args: &mut Args, mut end: End) -> Result<i32, Fail> {
    let mut force = false;
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('f') => force = true,
            Arg::Long(l) if l == "force" => force = true,
            Arg::Short('p') if end == End::Halt => end = End::PowerOff,
            Arg::Long(l) if l == "poweroff" => end = End::PowerOff,
            Arg::Long(l) if l == "reboot" => end = End::Reboot,
            Arg::Long(l) if l == "halt" => end = End::Halt,
            // Kept for scripts: there is no wtmp to write, and the kernel
            // syncs the disks whatever is asked.
            Arg::Short('w' | 'd' | 'n') => {}
            Arg::Long(l)
                if ["wtmp-only", "no-wtmp", "no-sync", "no-wall"].contains(&l.as_str()) => {}
            Arg::Long(l) if l == "help" => {
                println!(
                    "{} [OPTIONS...]\n\n{} the system.\n\n\
                     \x20    --halt      Halt the machine\n\
                     \x20 -p --poweroff  Switch off the machine\n\
                     \x20    --reboot    Reboot the machine\n\
                     \x20 -f --force     Force immediate {}, without stopping the services",
                    args.tool,
                    match end {
                        End::PowerOff => "Power off",
                        End::Reboot => "Reboot",
                        End::Halt => "Halt",
                    },
                    end.verb()
                );
                return Ok(0);
            }
            other => return Err(args.unknown(&other)),
        }
    }
    let request = if force { format!("{} --force", end.verb()) } else { end.verb().to_string() };
    ask(&request)?;
    Ok(0)
}

/// reboot(8).
pub fn reboot(args: &mut Args) -> Result<i32, Fail> {
    end_now(args, End::Reboot)
}

/// poweroff(8).
pub fn poweroff(args: &mut Args) -> Result<i32, Fail> {
    end_now(args, End::PowerOff)
}

/// halt(8).
pub fn halt(args: &mut Args) -> Result<i32, Fail> {
    end_now(args, End::Halt)
}

/// One line of Spark's status answer: a service.
#[derive(Debug, PartialEq, Eq)]
struct Status {
    name: String,
    state: String,
    pid: String,
    restarts: String,
    last: String,
    restart: String,
    description: String,
}

fn statuses(answer: &str) -> Vec<Status> {
    answer
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let f: Vec<&str> = line.splitn(7, '\t').collect();
            (f.len() == 7).then(|| Status {
                name: f[0].into(),
                state: f[1].into(),
                pid: f[2].into(),
                restarts: f[3].into(),
                last: f[4].into(),
                restart: f[5].into(),
                description: f[6].into(),
            })
        })
        .collect()
}

/// service(8): `service NAME start|stop|restart|status`, or
/// `service --status-all`.
pub fn service(args: &mut Args) -> Result<i32, Fail> {
    args.stop_at_operand();
    let mut operands = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Long(l) if l == "status-all" => {
                let answer = ask("status")?;
                for status in statuses(&answer) {
                    let mark = match status.state.as_str() {
                        "running" => '+',
                        "waiting" => '?',
                        _ => '-',
                    };
                    println!(" [ {mark} ]  {}", status.name);
                }
                return Ok(0);
            }
            Arg::Long(l) if l == "help" => {
                println!(
                    "Usage: service < option > | --status-all | [ service_name [ command | --full-restart ] ]"
                );
                return Ok(0);
            }
            Arg::Operand(value) => operands.push(value.to_string_lossy().into_owned()),
            other => return Err(args.unknown(&other)),
        }
    }
    operands.extend(args.rest().into_iter().map(|a| a.to_string_lossy().into_owned()));
    let (Some(name), Some(command)) = (operands.first(), operands.get(1)) else {
        return Err(usage(
            "Usage: service < option > | --status-all | [ service_name [ command | --full-restart ] ]"
                .into(),
        ));
    };
    match command.as_str() {
        "start" | "stop" | "restart" => {
            ask(&format!("{command} {name}"))?;
            Ok(0)
        }
        "--full-restart" => {
            ask(&format!("restart {name}"))?;
            Ok(0)
        }
        "status" => {
            let answer = ask("status")?;
            let Some(status) = statuses(&answer).into_iter().find(|s| &s.name == name) else {
                // LSB: 4 is an unknown service.
                return Err(fail_with(4, format!("{name}: unrecognized service")));
            };
            println!("{} - {}", status.name, status.description);
            let active = match status.state.as_str() {
                "running" => format!("active (running) as process {}", status.pid),
                "waiting" => "activating (waiting to start again)".into(),
                "disabled" => "inactive (disabled)".into(),
                _ => "inactive (dead)".into(),
            };
            println!("   Active: {active}");
            println!("  Restart: {}, {} restarts so far", status.restart, status.restarts);
            if status.last != "-" {
                println!("     Last: ended with wait status {}", status.last);
            }
            // LSB: 0 runs, 3 does not.
            Ok(if status.state == "running" { 0 } else { 3 })
        }
        other => {
            Err(usage(format!("{name}: unknown command `{other}' (start, stop, restart, status)")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(delay("now", (10, 0)), Some(0));
        assert_eq!(delay("+5", (10, 0)), Some(300));
        assert_eq!(delay("10:30", (10, 0)), Some(1800));
        // A time that has passed today is tomorrow's.
        assert_eq!(delay("09:00", (10, 0)), Some(23 * 3600));
        assert_eq!(delay("24:00", (10, 0)), None);
        assert_eq!(delay("soon", (10, 0)), None);
    }

    #[test]
    fn reads_spark_s_status() {
        let answer =
            "consoled\trunning\t12\t0\t-\talways\tVirtual consoles\n#\tshutdown\tpower off\t60\n";
        let statuses = statuses(answer);
        assert_eq!(statuses.len(), 1);
        assert_eq!((statuses[0].name.as_str(), statuses[0].pid.as_str()), ("consoled", "12"));
        assert_eq!(statuses[0].description, "Virtual consoles");
    }
}
