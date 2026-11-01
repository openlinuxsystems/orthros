// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Serial ports on USB (Phase 11g, ADR 0246): CDC-ACM interfaces, and
//! FTDI's chips (`radix_usb::serial`).
//!
//! Once a device is configured, [`Controller::start_serial`] makes each
//! of its serial ports a terminal, `/devices/ttyACM0` or
//! `/devices/ttyUSB0` and on, as Linux names them (`fs::tty::make_line`),
//! and starts a transfer on its bulk IN endpoint. The controller's thread
//! carries the port as it does a keyboard, and the terminal wakes the
//! thread whenever something changes ([`super::wake`]):
//!
//! - What comes on the bulk IN endpoint is typed on the terminal (without
//!   the two status bytes each of an FTDI chip's packets starts with);
//!   what the terminal cannot take yet is held, and the endpoint waits,
//!   until a program reads ([`Controller::serial_received`]).
//! - What programs write goes out on the bulk OUT endpoint, a page at a
//!   time ([`Controller::serial`]).
//! - The line is set as the terminal's settings say: SET_LINE_CODING for
//!   CDC-ACM, the baud rate's divisor, the character's size, parity and
//!   stop bits and the flow control for FTDI. DTR and RTS are raised
//!   while a program has the terminal open (not at B0), and lowered after
//!   the last close if HUPCL is set, as on Linux.
//!
//! A device unplugged takes its terminals away, which hang up. Only xHCI
//! controllers carry the bulk transfers a port needs.

use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, Ordering};

use radix_klog::{TextBuf, info, warn};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_usb::Setup;
use radix_usb::serial::{Line, Port, Protocol, acm, cflag, ftdi};
use radix_xhci::{BUFFER_SIZE, code, endpoint_index};

use super::controller::{Controller, Name, Why};
use crate::fs::tty::{self, TtyRef};

/// The most serial ports of a device that are carried.
pub(super) const MAX_SERIALS: usize = 4;
/// Errors in a row on a port's endpoint before it is given up.
const PIPE_ERRORS: u8 = 3;
/// The bytes each transfer on a bulk IN endpoint asks for, at most.
const READ_LENGTH: usize = 1024;
/// The bytes each transfer on a bulk OUT endpoint sends, at most.
const WRITE_LENGTH: usize = BUFFER_SIZE;

/// The numbers of `ttyACM` and `ttyUSB` terminals in use, a bit each.
static NUMBERS: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

/// What the thread keeps of a serial port it carries.
pub(super) struct Serial {
    port: Port,
    /// Its terminal.
    tty: TtyRef,
    /// Which of [`NUMBERS`] and which bit its name took.
    number: (usize, u32),
    /// Its bulk endpoints' device context indices.
    input: u8,
    output: u8,
    /// The transfers in flight on them, 0 for none.
    reading: u64,
    writing: u64,
    /// The bytes each read asks for: whole packets.
    read_length: usize,
    /// What came that the terminal has not taken yet.
    held: Vec<u8, Kmalloc>,
    /// The settings' generation last sent to the device.
    generation: u64,
    /// Whether DTR and RTS are raised.
    raised: bool,
    /// Errors in a row on its endpoints.
    errors: u8,
    /// Whether the line has been said to be at a speed the chip lacks.
    warned: bool,
}

/// Wakes the thread of the controller with index `index`, whose port's
/// terminal changed.
fn wake(index: usize) {
    super::wake(index);
}

/// A free number of `prefix`'s terminals, taken.
fn take_number(which: usize) -> Option<u32> {
    let numbers = &NUMBERS[which];
    let mut now = numbers.load(Ordering::Acquire);
    loop {
        let free = (!now).trailing_zeros();
        if free >= 32 {
            return None;
        }
        match numbers.compare_exchange(now, now | 1 << free, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(free),
            Err(seen) => now = seen,
        }
    }
}

impl Controller {
    /// Starts the serial port `port` of the configured device in slot
    /// `slot`: makes its terminal and starts reading it.
    #[inline(never)]
    pub(super) fn start_serial(&mut self, slot: u8, port: Port) -> Result<(), Why> {
        let at = self.find(slot).ok_or(Why::Device("it is gone"))?;
        let which = usize::from(matches!(port.protocol, Protocol::Ftdi { .. }));
        let number = take_number(which).ok_or(Why::Device("too many serial ports"))?;
        let mut name = TextBuf::<16>::new();
        let _ = write!(name, "{}{number}", port.prefix());
        let heap = HEAP.with(Flags::MAY_SLEEP);
        let made = (|| {
            let mut held = Vec::new_in(heap);
            held.try_reserve_exact(READ_LENGTH).map_err(|_| Why::Device("out of memory"))?;
            let tty = tty::make_line(name.as_str(), wake, self.bus - 1)
                .map_err(|_| Why::Device("its terminal could not be made"))?;
            Ok::<_, Why>((held, tty))
        })();
        let (held, tty) = match made {
            Ok(made) => made,
            Err(why) => {
                NUMBERS[which].fetch_and(!(1 << number), Ordering::AcqRel);
                return Err(why);
            }
        };
        let packet = usize::from(port.bulk_in.max_packet()).clamp(1, READ_LENGTH);
        let serial = Serial {
            port,
            tty,
            number: (which, number),
            input: endpoint_index(port.bulk_in.address),
            output: endpoint_index(port.bulk_out.address),
            reading: 0,
            writing: 0,
            read_length: READ_LENGTH / packet * packet,
            held,
            generation: 0,
            raised: true,
            errors: 0,
            warned: false,
        };
        let attached = &mut self.devices[at];
        if attached.serials.try_reserve(1).is_err() {
            serial.tty.line_gone();
            NUMBERS[which].fetch_and(!(1 << number), Ordering::AcqRel);
            return Err(Why::Device("out of memory"));
        }
        let what = match port.protocol {
            Protocol::Acm { .. } => "a CDC-ACM serial port",
            Protocol::Ftdi { chip, .. } => chip.name(),
        };
        info!(
            "USB: {}: interface {}: {what}, the terminal /devices/{name}",
            Name(self.bus, attached.path),
            port.data
        );
        let _ = attached.serials.push_within_capacity(serial);
        let index = attached.serials.len() - 1;
        if let Protocol::Ftdi { index: wanted, .. } = port.protocol {
            let _ = self.devices[at].device.control_out(
                &mut self.xhci,
                &mut self.host,
                ftdi::reset(wanted),
                &[],
            );
        }
        // The line set, and its modem lines lowered until it is opened.
        self.serial_settings(at, index);
        self.serial_listen(at, index)
    }

    /// Starts the read on serial port `index` of the device at `at`.
    fn serial_listen(&mut self, at: usize, index: usize) -> Result<(), Why> {
        let attached = &mut self.devices[at];
        let Some(serial) = attached.serials.get_mut(index) else { return Ok(()) };
        if serial.reading == 0 {
            serial.reading =
                attached.device.submit_in(&self.xhci, serial.input, serial.read_length)?;
        }
        Ok(())
    }

    /// Sends serial port `index` of the device at `at` its terminal's
    /// settings if they changed, and raises or lowers its modem lines as
    /// its terminal is open or not.
    #[inline(never)]
    fn serial_settings(&mut self, at: usize, index: usize) {
        let Controller { devices, xhci, host, bus, .. } = self;
        let attached = &mut devices[at];
        let path = attached.path;
        let Some(serial) = attached.serials.get_mut(index) else { return };
        let (generation, flags, ospeed) = serial.tty.line_settings();
        let line = Line::from_cflag(flags, ospeed);
        let device = &mut attached.device;
        if generation != serial.generation {
            serial.generation = generation;
            match serial.port.protocol {
                Protocol::Acm { control } if line.rate > 0 => {
                    let coding = acm::line_coding(&line);
                    let _ =
                        device.control_out(xhci, host, Setup::set_line_coding(control), &coding);
                }
                Protocol::Ftdi { chip, index } if line.rate > 0 => {
                    match ftdi::baud_rate(chip, index, line.rate) {
                        Some(setup) => {
                            let _ = device.control_out(xhci, host, setup, &[]);
                        }
                        None if !serial.warned => {
                            serial.warned = true;
                            warn!(
                                "USB: {}: the {} cannot run at {} baud",
                                Name(*bus, path),
                                chip.name(),
                                line.rate
                            );
                        }
                        None => {}
                    }
                    let _ = device.control_out(xhci, host, ftdi::data(index, &line), &[]);
                    let flow = ftdi::flow_control(index, line.rts_cts);
                    let _ = device.control_out(xhci, host, flow, &[]);
                }
                _ => {}
            }
        }
        // Raised while open, unless the speed is 0; lowered on the last
        // close only with HUPCL.
        let open = serial.tty.line_open();
        let raise = if open { line.rate > 0 } else { serial.raised && flags & cflag::HUPCL == 0 };
        if raise != serial.raised {
            serial.raised = raise;
            let setup = match serial.port.protocol {
                Protocol::Acm { control } => {
                    let lines = if raise { acm::DTR | acm::RTS } else { 0 };
                    Setup::set_control_lines(control, lines)
                }
                Protocol::Ftdi { index, .. } => ftdi::modem_control(index, raise),
            };
            let _ = device.control_out(xhci, host, setup, &[]);
        }
    }

    /// Carries every serial port: their settings and modem lines, what
    /// they hold for their terminals, and what their terminals have to
    /// send; says whether anything moved.
    pub(super) fn serial(&mut self) -> bool {
        let mut any = false;
        for at in 0..self.devices.len() {
            for index in 0..self.devices[at].serials.len() {
                self.serial_settings(at, index);
                any |= self.serial_give(at, index);
                any |= self.serial_send(at, index);
            }
        }
        any
    }

    /// Gives serial port `index`'s terminal what it holds; once it holds
    /// nothing, reads again. Says whether the terminal took any.
    fn serial_give(&mut self, at: usize, index: usize) -> bool {
        let Some(serial) = self.devices[at].serials.get_mut(index) else { return false };
        if serial.held.is_empty() {
            return false;
        }
        let taken = serial.tty.line_give(&serial.held);
        let left = serial.held.len() - taken;
        serial.held.copy_within(taken.., 0);
        serial.held.truncate(left);
        if serial.held.is_empty()
            && let Err(why) = self.serial_listen(at, index)
        {
            warn!(
                "USB: {}: a serial port cannot be read: {why}",
                Name(self.bus, self.devices[at].path)
            );
        }
        taken > 0
    }

    /// Sends what serial port `index`'s terminal has to send, if nothing
    /// is being sent. Says whether it did.
    fn serial_send(&mut self, at: usize, index: usize) -> bool {
        let Controller { devices, buffer, xhci, bus, .. } = self;
        let attached = &mut devices[at];
        let Some(serial) = attached.serials.get_mut(index) else { return false };
        if serial.writing != 0 || !serial.tty.line_output() {
            return false;
        }
        let n = serial.tty.line_take(&mut buffer[..WRITE_LENGTH]);
        if n == 0 {
            return false;
        }
        match attached.device.submit_out(xhci, serial.output, &buffer[..n]) {
            Ok(trb) => serial.writing = trb,
            Err(err) => {
                warn!(
                    "USB: {}: what a serial port was to send is lost: {err}",
                    Name(*bus, attached.path)
                );
            }
        }
        true
    }

    /// Whether the transfer at `trb` on `endpoint` of the device at `at`
    /// is one of its serial ports'; if so, takes what it brought.
    pub(super) fn serial_transfer(
        &mut self,
        at: usize,
        trb: u64,
        endpoint: u8,
        code: u8,
        residual: u32,
    ) -> bool {
        let serials = &self.devices[at].serials;
        let Some(index) = serials.iter().position(|s| {
            (s.input == endpoint && s.reading == trb) || (s.output == endpoint && s.writing == trb)
        }) else {
            return false;
        };
        if serials[index].input == endpoint && serials[index].reading == trb {
            self.serial_received(at, index, code, residual);
        } else {
            self.serial_sent(at, index, code);
        }
        true
    }

    /// The read on serial port `index` of the device at `at` finished with
    /// `code`, `residual` bytes short: its terminal is given what came,
    /// and it reads again once that is all taken.
    #[inline(never)]
    fn serial_received(&mut self, at: usize, index: usize, code: u8, residual: u32) {
        let Controller { devices, buffer, xhci, host, bus, .. } = self;
        let attached = &mut devices[at];
        let path = attached.path;
        let Some(serial) = attached.serials.get_mut(index) else { return };
        serial.reading = 0;
        match code {
            code::SUCCESS | code::SHORT_PACKET => {
                serial.errors = 0;
                let came = serial.read_length.saturating_sub(residual as usize);
                if attached.device.received(serial.input, &mut buffer[..came]).is_err() {
                    return;
                }
                let data = match serial.port.protocol {
                    Protocol::Ftdi { .. } => {
                        let packet = usize::from(serial.port.bulk_in.max_packet());
                        let (raw, rest) = buffer.split_at_mut(came);
                        let n = ftdi::strip(raw, packet, rest);
                        // Moved to the front, where the bytes came.
                        buffer.copy_within(came..came + n, 0);
                        &buffer[..n]
                    }
                    Protocol::Acm { .. } => &buffer[..came],
                };
                let taken = serial.tty.line_give(data);
                // Held until the terminal has room; the reads wait.
                for &byte in &data[taken..] {
                    let _ = serial.held.push_within_capacity(byte);
                }
                if !serial.held.is_empty() {
                    return;
                }
            }
            code::STOPPED => return,
            failed => {
                serial.errors += 1;
                if serial.errors > PIPE_ERRORS {
                    warn!(
                        "USB: {}: serial port {} stopped: {}",
                        Name(*bus, path),
                        serial.port.data,
                        code::name(failed)
                    );
                    return;
                }
                let endpoint = serial.input;
                if attached.device.recover(xhci, host, endpoint, failed).is_err() {
                    return;
                }
            }
        }
        if let Err(why) = self.serial_listen(at, index) {
            warn!("USB: {}: a serial port cannot be read: {why}", Name(self.bus, path));
        }
    }

    /// The write on serial port `index` of the device at `at` finished with
    /// `code`.
    fn serial_sent(&mut self, at: usize, index: usize, code: u8) {
        let Controller { devices, xhci, host, bus, .. } = self;
        let attached = &mut devices[at];
        let path = attached.path;
        let Some(serial) = attached.serials.get_mut(index) else { return };
        serial.writing = 0;
        match code {
            code::SUCCESS | code::SHORT_PACKET => serial.errors = 0,
            code::STOPPED => {}
            failed => {
                serial.errors += 1;
                warn!(
                    "USB: {}: serial port {} lost what it was sending: {}",
                    Name(*bus, path),
                    serial.port.data,
                    code::name(failed)
                );
                let endpoint = serial.output;
                let _ = attached.device.recover(xhci, host, endpoint, failed);
            }
        }
    }
}

/// The ports `serials`, of a device that is gone: their terminals go,
/// and hang up.
pub(super) fn gone(serials: &mut Vec<Serial, Kmalloc>) {
    while let Some(serial) = serials.pop() {
        serial.tty.line_gone();
        let (which, number) = serial.number;
        NUMBERS[which].fetch_and(!(1 << number), Ordering::AcqRel);
    }
}

/// An empty list of serial ports, for a device just set up.
pub(super) fn none() -> Vec<Serial, Kmalloc> {
    Vec::new_in(HEAP.with(Flags::MAY_SLEEP))
}
