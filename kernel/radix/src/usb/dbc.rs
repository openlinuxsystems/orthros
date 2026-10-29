// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The console over the xHCI Debug Capability (ADR 0160), for a machine
//! with no serial port: with the `dbc` flag, the first controller that has
//! one turns it on, and a second computer plugged into one of its USB 3
//! ports with a debug cable reads the kernel log from it, and talks to
//! `kdb`. A Linux debug host sees a serial port, `/dev/ttyUSB0`.
//!
//! The log goes to it through a sink that only copies into [`OUTPUT`], a
//! ring of the last 64 KiB (the oldest bytes give way), since the sinks
//! run with the log's lock held. The controller's thread polls the
//! capability every 10 ms ([`poll`]), which moves it on, starts a write of
//! what waits, and keeps what the debug host typed in [`INPUT`] for `kdb`.
//! `kdb`, which may run while the thread cannot, and the panic path, which
//! runs with the other CPUs stopped, poll it themselves
//! ([`crate::debug_console`]). The locks are taken in one order, the
//! capability's ([`DBC`]) before a ring's, and nothing is logged while one
//! is held.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use radix_hal::Timer;
use radix_klog::{Sink, Style, error, info, warn};
use radix_sync::SpinLock;
use radix_xhci::Xhci;
use radix_xhci::dbc::{Change, Dbc, Identity, Status};

use super::controller::alloc;
use crate::Arch;

/// What the debug host is told it is: Linux's own Debug Capability's IDs,
/// which Linux's `usb_debug` driver takes as a serial port.
const IDENTITY: Identity = Identity {
    vendor: 0x1d6b,
    product: 0x0010,
    revision: 0x0010,
    protocol: 1,
    manufacturer: "Dukovo",
    product_name: "Radix debug console",
    serial: "0001",
};

/// The bytes kept for the debug host, and those it sent.
const OUTPUT_BYTES: usize = 64 * 1024;
const INPUT_BYTES: usize = 256;
/// How long the panic path waits for the report to go out.
const FLUSH_NS: u64 = 2_000_000_000;
/// How long `kdb` waits at the end of boot for the controllers to look
/// for a Debug Capability.
const LOOK_NS: u64 = 5_000_000_000;

/// Set by the `dbc` flag.
static ON: AtomicBool = AtomicBool::new(false);
/// The bus whose capability is the console, or 0.
static BUS: AtomicUsize = AtomicUsize::new(0);
/// Controllers that have looked for one.
static LOOKED: AtomicUsize = AtomicUsize::new(0);

/// The capability, once a controller has turned it on.
static DBC: SpinLock<Option<Dbc>> = SpinLock::new(None);
/// What waits to go to the debug host, and what it sent.
static OUTPUT: SpinLock<Bytes<OUTPUT_BYTES>> = SpinLock::new(Bytes::new());
static INPUT: SpinLock<Bytes<INPUT_BYTES>> = SpinLock::new(Bytes::new());

/// A ring of bytes; a byte pushed when it is full pushes out the oldest.
struct Bytes<const N: usize> {
    data: [u8; N],
    start: usize,
    len: usize,
}

impl<const N: usize> Bytes<N> {
    const fn new() -> Self {
        Bytes { data: [0; N], start: 0, len: 0 }
    }

    fn push(&mut self, byte: u8) {
        if self.len == N {
            self.start = (self.start + 1) % N;
            self.len -= 1;
        }
        self.data[(self.start + self.len) % N] = byte;
        self.len += 1;
    }

    fn pop(&mut self) -> Option<u8> {
        if self.len == 0 {
            return None;
        }
        let byte = self.data[self.start];
        self.consume(1);
        Some(byte)
    }

    /// The oldest bytes, as many as lie in one piece.
    fn front(&self) -> &[u8] {
        let end = (self.start + self.len).min(N);
        &self.data[self.start..end]
    }

    fn consume(&mut self, count: usize) {
        let count = count.min(self.len);
        self.start = (self.start + count) % N;
        self.len -= count;
    }
}

/// The log's sink: copies the text, with `\n` made `\r\n` as the serial
/// port has it. The log holds interrupts off around its sinks.
struct DbcSink;

impl Sink for DbcSink {
    fn write(&self, text: &str, _style: Style) {
        push(&mut OUTPUT.lock(), text);
    }
}

static SINK: DbcSink = DbcSink;

fn push(output: &mut Bytes<OUTPUT_BYTES>, text: &str) {
    for byte in text.bytes() {
        if byte == b'\n' {
            output.push(b'\r');
        }
        output.push(byte);
    }
}

fn now_us() -> u64 {
    Arch::now_ns() / 1000
}

/// Reads the `dbc` flag.
pub fn init(on: bool) {
    ON.store(on, Ordering::Relaxed);
}

/// Turns on the Debug Capability of `xhci`, bus `bus`, as the console, if
/// the `dbc` flag asks for one and no other controller's is; says whether
/// it did, and its thread now polls it.
#[inline(never)]
pub(super) fn claim(bus: usize, xhci: &Xhci) -> bool {
    if !ON.load(Ordering::Relaxed) {
        return false;
    }
    if xhci.info().debug_capability.is_none() {
        info!("USB: bus {bus} has no Debug Capability, so the console is not on it");
        return false;
    }
    if BUS.compare_exchange(0, bus, Ordering::AcqRel, Ordering::Acquire).is_err() {
        info!("USB: bus {bus}'s Debug Capability is left off: the console is on another bus");
        return false;
    }
    let mut dbc = match Dbc::new(xhci, IDENTITY, &mut alloc) {
        Ok(dbc) => dbc,
        Err(err) => {
            warn!("USB: bus {bus}: the Debug Capability could not be set up: {err}");
            BUS.store(0, Ordering::Release);
            return false;
        }
    };
    dbc.start(now_us());
    *DBC.lock_irq() = Some(dbc);
    radix_klog::add_sink(&SINK, true);
    true
}

/// A controller has looked for a Debug Capability, whether or not it
/// could start.
pub(super) fn looked() {
    LOOKED.fetch_add(1, Ordering::AcqRel);
}

/// Moves the capability on, from its controller's thread, and logs what
/// changed.
pub(super) fn poll(bus: usize) {
    let change = {
        let mut dbc = DBC.lock_irq();
        let Some(dbc) = dbc.as_mut() else { return };
        let change = dbc.poll(now_us());
        transfer(dbc);
        change
    };
    if let Some(change) = change {
        report(bus, change);
    }
}

/// Starts a write of what waits, and takes what came.
fn transfer(dbc: &mut Dbc) {
    if dbc.status() != Status::Running {
        return;
    }
    if !dbc.writing() {
        let mut output = OUTPUT.lock_irq();
        let taken = dbc.write(output.front());
        output.consume(taken);
    }
    let mut input = INPUT.lock_irq();
    let mut bytes = [0u8; 64];
    loop {
        let room = (INPUT_BYTES - input.len).min(bytes.len());
        let came = dbc.read(&mut bytes[..room]);
        if came == 0 {
            break;
        }
        for &byte in &bytes[..came] {
            input.push(byte);
        }
    }
}

#[inline(never)]
fn report(bus: usize, change: Change) {
    match change {
        Change::Enabled => {
            info!("USB: bus {bus}: the Debug Capability is on, waiting for a debug host");
        }
        Change::Connected => info!("USB: bus {bus}: a debug host is connected"),
        Change::Configured { port } => {
            info!("USB: bus {bus}: the debug host on port {port} reads the console");
        }
        Change::Disconnected { reset: false } => info!("USB: bus {bus}: the debug host went away"),
        Change::Disconnected { reset: true } => {
            info!("USB: bus {bus}: the debug host reset its port");
        }
        Change::TurnedOff => {
            warn!("USB: bus {bus}: the Debug Capability was turned off; it is set up again");
        }
        Change::NotConfigured => warn!(
            "USB: bus {bus}: the debug host did not configure the Debug Capability in 5 s; it is turned off and on"
        ),
        Change::Failed(why) => error!("USB: bus {bus}: the Debug Capability failed: {why}"),
    }
}

/// Turns the capability off, when its controller stopped or before the
/// machine powers off or restarts.
pub(super) fn stop() {
    if let Some(dbc) = DBC.lock_irq().as_mut() {
        dbc.stop();
    }
}

/// Whether the console is on a Debug Capability.
pub fn console() -> bool {
    BUS.load(Ordering::Acquire) != 0
}

/// At the end of boot, for `kdb`: waits for the controllers to look for a
/// Debug Capability, if the `dbc` flag asks for one; says whether the
/// console is on one.
pub fn wait_for_console() -> bool {
    if !ON.load(Ordering::Relaxed) {
        return false;
    }
    let deadline = Arch::now_ns().saturating_add(LOOK_NS);
    while !console()
        && LOOKED.load(Ordering::Acquire) < super::COUNT.load(Ordering::Acquire)
        && Arch::now_ns() < deadline
    {
        Arch::delay_us(1000);
    }
    console()
}

/// Moves the capability on from wherever its thread may not run: `kdb`,
/// and the panic path, whose stopped CPUs may hold the lock (panic mode
/// takes it anyway).
fn pump() {
    let guard = if radix_sync::in_panic_mode() { Some(DBC.lock_irq()) } else { DBC.try_lock_irq() };
    let Some(mut guard) = guard else { return };
    if let Some(dbc) = guard.as_mut() {
        let _ = dbc.poll(now_us());
        transfer(dbc);
    }
}

/// Writes `text` to the debug host, for `kdb` and the panic path.
pub fn write(text: &str) {
    if !console() {
        return;
    }
    push(&mut OUTPUT.lock_irq(), text);
    pump();
}

/// A byte the debug host typed, if one waits.
pub fn read() -> Option<u8> {
    if !console() {
        return None;
    }
    pump();
    INPUT.lock_irq().pop()
}

/// Sends what waits, the panic report, for up to 2 s.
pub fn flush() {
    if !console() {
        return;
    }
    let deadline = Arch::now_ns().saturating_add(FLUSH_NS);
    while Arch::now_ns() < deadline {
        pump();
        let busy = DBC.lock_irq().as_ref().is_some_and(|dbc| {
            dbc.status() == Status::Running && (dbc.writing() || OUTPUT.lock_irq().len > 0)
        });
        if !busy {
            return;
        }
        Arch::delay_us(100);
    }
}
