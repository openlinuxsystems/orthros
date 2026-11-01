// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! USB-C ports, through the firmware's UCSI device (`radix_ucsi`, Phase
//! 11g, ADR 0248).
//!
//! [`find`] looks for the namespace's `USBC000` device (or one whose
//! `_CID` is `PNP0CA0`), maps the mailbox its `_CRS` names and resets the
//! PPM behind it, then asks what it can do and what is on each port, and
//! logs it: a charger and the power it gives, a device the machine
//! powers, a dock. The firmware notifies the device when a port changes;
//! the `usb-c` thread then reads what changed, logs it, and acknowledges
//! it. `/kernel/class/typec` shows each port in Linux's layout, and
//! `/kernel/class/power_supply` the power each takes (`fs::sysfs`), as
//! Linux's `ucsi_acpi` has them.
//!
//! Commands go one at a time, under [`PPM`]'s lock, and run AML (the
//! `_DSM` that passes the mailbox), so they come only from threads that
//! may sleep.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};

use radix_acpi::resources::{Memory, Resource, Resources};
use radix_hal::Devices;
use radix_hw::Mmio;
use radix_klog::{error, info, warn};
use radix_sync::{Mutex, Once, SpinLock};
use radix_uacpi::Node;
use radix_ucsi::{
    ConnectorCapability, ConnectorStatus, Contract, Describe, Error, Mailbox, NOTIFICATIONS, Pdo,
    PowerMode, Ppm,
};

use crate::Arch;
use crate::sched::{self, Options, Policy, ThreadRef};

/// The most ports kept.
const MAX_PORTS: usize = 8;

/// The mailbox in memory, and the device whose `_DSM` passes it.
pub struct Firmware {
    node: Node,
    mailbox: Mmio,
}

impl Mailbox for Firmware {
    fn read(&mut self, offset: usize, out: &mut [u8]) {
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = self.mailbox.read8(offset + i);
        }
    }

    fn write(&mut self, offset: usize, data: &[u8]) {
        for (i, &byte) in data.iter().enumerate() {
            self.mailbox.write8(offset + i, byte);
        }
    }

    fn after_write(&mut self) -> Result<(), Error> {
        let (uuid, revision) = (&radix_ucsi::DSM_UUID, radix_ucsi::DSM_REVISION);
        self.node.dsm_run(uuid, revision, radix_ucsi::DSM_WRITE).map_err(|_| Error::Firmware)
    }

    fn before_read(&mut self) -> Result<(), Error> {
        let (uuid, revision) = (&radix_ucsi::DSM_UUID, radix_ucsi::DSM_REVISION);
        self.node.dsm_run(uuid, revision, radix_ucsi::DSM_READ).map_err(|_| Error::Firmware)
    }

    fn delay_ms(&mut self, ms: u32) {
        sched::sleep_ns(u64::from(ms) * 1_000_000);
    }
}

/// A port, and what was last read of it.
pub struct Port {
    /// Its number, from 1, as the firmware counts them; Linux names it
    /// `port{number - 1}`.
    pub number: u8,
    pub capability: ConnectorCapability,
    last: SpinLock<Reading>,
}

/// What a port had when last read.
#[derive(Clone, Copy, Default)]
pub struct Reading {
    pub status: ConnectorStatus,
    /// Its Power Delivery contract, and the most current its object
    /// allows, when known.
    pub contract: Option<Contract>,
    pub max_ma: Option<u32>,
}

/// The PPM, once set up.
static PPM: Mutex<Option<Ppm<Firmware>>> = Mutex::new(None);
static PORTS: [Once<Port>; MAX_PORTS] = [const { Once::new() }; MAX_PORTS];
static THREAD: Once<ThreadRef> = Once::new();
/// The firmware notified a change the thread has not looked at yet.
static NOTIFIED: AtomicBool = AtomicBool::new(false);

/// The ports found.
pub fn ports() -> impl Iterator<Item = &'static Port> {
    PORTS.iter().filter_map(Once::get)
}

impl Port {
    /// What it had when last read.
    pub fn reading(&self) -> Reading {
        *self.last.lock()
    }
}

/// Finds the UCSI device and sets its ports up. Called once uACPI runs,
/// from a thread that may sleep.
pub fn find() {
    let mut found = None;
    for id in radix_ucsi::IDS {
        radix_uacpi::find_devices(id, &mut |node| {
            if found.is_none() && node.present() {
                found = Some(node);
            }
        });
    }
    let Some(node) = found else { return };
    if let Err(why) = set_up(node) {
        warn!(
            "USB-C: the firmware's UCSI device {}: {why}; its ports are not watched",
            node.path()
        );
    }
}

/// Why setting up failed.
enum Failed {
    NoMailbox,
    Unmapped,
    Ppm(&'static str, Error),
    Thread,
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failed::NoMailbox => f.write_str("its _CRS names no mailbox in memory"),
            Failed::Unmapped => f.write_str("its mailbox could not be mapped"),
            Failed::Ppm(what, err) => write!(f, "{what}: {err}"),
            Failed::Thread => f.write_str("the thread `usb-c` could not start"),
        }
    }
}

/// Maps the mailbox of `node`, resets the PPM and reads its ports.
#[inline(never)]
fn set_up(node: Node) -> Result<(), Failed> {
    let memory = node
        .bytes(c"_CRS", |bytes| {
            Resources::new(bytes).find_map(|resource| match resource {
                Ok(Resource::Memory(memory)) => Some(memory),
                _ => None,
            })
        })
        .ok()
        .flatten()
        .ok_or(Failed::NoMailbox)?;
    let Memory { base, length, .. } = memory;
    let length = usize::try_from(length).unwrap_or(0).max(radix_ucsi::MAILBOX_LEN);
    let mailbox = <Arch as Devices>::map_device(base, length).ok_or(Failed::Unmapped)?;
    let mut ppm = Ppm::reset(Firmware { node, mailbox })
        .map_err(|err| Failed::Ppm("it did not reset", err))?;
    if let Err(err) = ppm.enable_notifications(NOTIFICATIONS) {
        warn!(
            "USB-C: the firmware will not tell of changes ({err}); the ports are read at boot only"
        );
    }
    let capability =
        ppm.capability().map_err(|err| Failed::Ppm("it would not say what it can do", err))?;
    let version = ppm.version();
    info!(
        "USB-C: the firmware's UCSI {}.{} interface at {}: {} {}{}",
        version >> 8,
        version >> 4 & 0xf,
        node.path(),
        capability.connectors,
        if capability.connectors == 1 { "port" } else { "ports" },
        if capability.power_delivery() { ", USB Power Delivery" } else { "" },
    );
    let count = usize::from(capability.connectors).min(MAX_PORTS);
    if usize::from(capability.connectors) > MAX_PORTS {
        warn!("USB-C: only the first {MAX_PORTS} ports are watched");
    }
    for (index, slot) in PORTS.iter().enumerate().take(count) {
        let number = index as u8 + 1;
        let capability = match ppm.connector_capability(number) {
            Ok(capability) => capability,
            Err(err) => {
                warn!("USB-C: port {number}: the firmware would not say what it can do: {err}");
                continue;
            }
        };
        let port = Port { number, capability, last: SpinLock::new(Reading::default()) };
        let port = slot.call_once(|| port);
        read_port(&mut ppm, port, true);
    }
    *PPM.lock() = Some(ppm);
    if let Err(err) = node.on_notify(notified) {
        warn!("USB-C: changes cannot be heard ({err}); the ports are read at boot only");
        return Ok(());
    }
    let options = Options::new().policy(Policy::Fair { nice: -5 });
    let thread = sched::spawn("usb-c", run, 0, options).map_err(|_| Failed::Thread)?;
    let _ = THREAD.set(thread);
    // A change the firmware told of before the thread was there.
    wake();
    Ok(())
}

/// Reads what is on `port`, logs it if it is new or `always`, and
/// acknowledges the change it shows.
fn read_port(ppm: &mut Ppm<Firmware>, port: &Port, always: bool) {
    let number = port.number;
    let status = match ppm.connector_status(number) {
        Ok(status) => status,
        Err(err) => {
            warn!("USB-C: port {number}: the firmware would not say what is on it: {err}");
            return;
        }
    };
    let (contract, max_ma) =
        if status.connected { power_of(ppm, number, &status) } else { (None, None) };
    if status.change != 0
        && let Err(err) = ppm.ack_change(number)
    {
        warn!("USB-C: port {number}: the change could not be acknowledged: {err}");
    }
    let reading = Reading { status, contract, max_ma };
    let before = core::mem::replace(&mut *port.last.lock(), reading);
    let same = before.status.connected == status.connected
        && before.status.partner == status.partner
        && before.status.provider == status.provider
        && before.status.power == status.power
        && before.contract == contract
        && before.status.charging == status.charging;
    if always || !same {
        info!("USB-C: port {number}: {}", Describe { status: &status, contract });
    }
}

/// The contract on a connected port, and the most current the object it
/// is for allows.
fn power_of(
    ppm: &mut Ppm<Firmware>,
    number: u8,
    status: &ConnectorStatus,
) -> (Option<Contract>, Option<u32>) {
    let contract = ppm.contract(number, status);
    let position = usize::from(status.object_position());
    let mut pdos = [0; 4];
    let most = match ppm.pdos(number, !status.provider, true, &mut pdos) {
        Ok(n) if (1..=n).contains(&position) => match Pdo::parse(pdos[position - 1]) {
            Pdo::Fixed { ma, .. } | Pdo::Variable { ma, .. } | Pdo::Programmable { ma, .. } => {
                Some(ma)
            }
            _ => None,
        },
        _ => None,
    };
    (contract, most)
}

/// In the notify thread: a port changed.
fn notified(_: Node, _: u64) {
    wake();
}

fn wake() {
    NOTIFIED.store(true, Ordering::Release);
    if let Some(thread) = THREAD.get() {
        sched::wake(thread);
    }
}

/// The `usb-c` thread: reads each port the firmware says changed.
fn run(_: usize) {
    loop {
        if !NOTIFIED.swap(false, Ordering::AcqRel) {
            sched::block("usb-c");
            continue;
        }
        let mut guard = PPM.lock();
        let Some(ppm) = guard.as_mut() else { continue };
        // At most a change per port, so that a firmware that never stops
        // saying one changed does not keep the thread.
        for _ in 0..MAX_PORTS {
            match ppm.changed() {
                Ok(Some(number)) => {
                    match ports().find(|port| port.number == number) {
                        Some(port) => read_port(ppm, port, false),
                        // A port not kept: acknowledged, so that the
                        // firmware goes on.
                        None => {
                            let _ = ppm.ack_change(number);
                        }
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    error!("USB-C: the firmware would not say what changed: {err}");
                    break;
                }
            }
        }
    }
}

/// Writes Linux's `data_role` of `port`: both roles, the one it has now
/// in brackets, for a dual-role port.
pub fn write_data_role(port: &Port, text: &mut dyn Write) -> fmt::Result {
    let role = port.reading().status.data_role();
    match port.capability.port_type() {
        "dual" if role == "host" => writeln!(text, "[host] device"),
        "dual" => writeln!(text, "host [device]"),
        "source" => writeln!(text, "host"),
        _ => writeln!(text, "device"),
    }
}

/// Writes Linux's `power_role` of `port`, likewise.
pub fn write_power_role(port: &Port, text: &mut dyn Write) -> fmt::Result {
    let role = port.reading().status.power_role();
    let capability = port.capability;
    match (capability.provider, capability.consumer) {
        (true, true) if role == "source" => writeln!(text, "[source] sink"),
        (true, true) => writeln!(text, "source [sink]"),
        (true, false) => writeln!(text, "source"),
        _ => writeln!(text, "sink"),
    }
}

/// Writes Linux's `port_type` of `port`.
pub fn write_port_type(port: &Port, text: &mut dyn Write) -> fmt::Result {
    match port.capability.port_type() {
        "dual" => writeln!(text, "[dual] source sink"),
        kind => writeln!(text, "{kind}"),
    }
}

/// The power `port` takes: (online, µV, µA now, most µA), as Linux's
/// `ucsi-source-psy` supply has it. Without Power Delivery, the supply
/// is 5 V at what its mode allows.
pub fn supply(port: &Port) -> (bool, u64, u64, u64) {
    let Reading { status, contract, max_ma } = port.reading();
    let online = status.connected && !status.provider;
    if !online {
        return (false, 0, 0, 0);
    }
    let (mv, ma, most) = match (status.power, contract) {
        (PowerMode::PowerDelivery, Some(contract)) => {
            (contract.mv, contract.ma, max_ma.unwrap_or(contract.ma))
        }
        (PowerMode::TypeC1_5 | PowerMode::BatteryCharging, _) => (5000, 1500, 1500),
        (PowerMode::TypeC3_0, _) => (5000, 3000, 3000),
        (PowerMode::TypeC5_0, _) => (5000, 5000, 5000),
        _ => (5000, 500, 500),
    };
    let micro = |milli: u32| u64::from(milli) * 1000;
    (true, micro(mv), micro(ma), micro(most))
}
