// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! USB sticks, card readers and USB disks: mass storage interfaces of the
//! Bulk-Only Transport (Phase 11b, ADR 0161), as the controller's thread
//! carries them.
//!
//! Once a device is configured, [`Controller::start_storage`] starts a
//! kernel thread for its mass storage interface, `usb-storage`, as Linux
//! has one: it finds the disks behind the interface and serves their
//! requests (`fs::disk::usb`). Each command goes in stages, a transfer
//! each, which the stick's thread waits for; only the controller's thread
//! drives the controller, so the stick's thread asks it for each
//! ([`Link`], through [`StickPipes`]): a transfer on a bulk pipe, straight
//! to or from the buffers it names (`radix_xhci::Device::submit_frames`),
//! which the controller's thread starts, and whose event hands back how
//! much moved ([`Controller::storage_transfer`]); a pipe's halt cleared;
//! a control transfer (Get Max LUN, the class's reset); or a transfer
//! that took too long stopped. The controller's thread looks for what was
//! asked each time it wakes, and the stick's thread wakes it for each
//! ask. A device unplugged, or a controller that fails, fails what was
//! asked and every ask after ([`Link::gone`]), which ends the stick's
//! thread and takes its disks away.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use radix_hw::{Dma, PAGE_SIZE};
use radix_klog::{TextBuf, info};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_scsi::Data;
use radix_sync::SpinLock;
use radix_usb::storage::{PROTOCOL_BULK_ONLY, PROTOCOL_UAS, PipeError, Pipes, SUBCLASS_SCSI};
use radix_usb::{EndpointDescriptor, InterfaceDescriptor, Setup, Speed, TransferType, descriptor};
use radix_xhci::{MAX_TD_BUFFERS, Td, code, endpoint_index};

use super::controller::{Controller, Name, Why};
use crate::fs::disk::SERIAL;
use crate::sched::{self, Options, Policy, ThreadRef};

/// The most sticks driven at once.
const MAX_STICKS: usize = 16;
/// How often a stick's thread that waits for its controller's looks
/// whether the stick is gone, at most.
const LOOK_NS: u64 = 1_000_000_000;

/// A mass storage interface in a device's configuration.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct StoragePlan {
    /// bInterfaceNumber.
    pub(super) interface: u8,
    /// Its subclass and protocol, which say how its commands go.
    subclass: u8,
    protocol: u8,
    /// Its bulk IN and OUT endpoints' addresses.
    bulk_in: Option<u8>,
    bulk_out: Option<u8>,
    /// Its device's serial number string, and the language its strings
    /// are read in.
    serial: u8,
    language: u16,
}

impl StoragePlan {
    /// The mass storage interface `interface`, with its `endpoints`, of a
    /// device whose serial number is string `serial`, its strings read in
    /// `language`.
    pub(super) fn of(
        interface: InterfaceDescriptor,
        endpoints: impl Iterator<Item = EndpointDescriptor>,
        (serial, language): (u8, u16),
    ) -> StoragePlan {
        let mut plan = StoragePlan {
            interface: interface.number,
            subclass: interface.subclass,
            protocol: interface.protocol,
            bulk_in: None,
            bulk_out: None,
            serial,
            language,
        };
        for endpoint in endpoints.filter(|e| e.transfer_type() == TransferType::Bulk) {
            let pipe = if endpoint.is_in() { &mut plan.bulk_in } else { &mut plan.bulk_out };
            pipe.get_or_insert(endpoint.address);
        }
        plan
    }
}

/// What a stick's thread asks of its controller's thread.
#[derive(Clone, Copy, Debug)]
enum Ask {
    /// A transfer of the buffers in [`Mail::frames`] on the bulk endpoint
    /// with this device context index.
    Bulk(u8),
    /// The halt of that endpoint cleared, on both sides.
    ClearHalt(u8),
    /// A control transfer, its data in [`Mail::control`].
    Control(Setup),
    /// The transfer started for the last [`Ask::Bulk`] stopped, unless it
    /// has ended.
    Cancel,
}

/// What passes between a stick's thread and its controller's.
struct Mail {
    /// What was asked, not yet taken.
    ask: Option<Ask>,
    /// The transfer started for it: its endpoint, and where it is.
    started: Option<(u8, Td)>,
    /// How what was asked ended: the bytes that moved.
    answer: Option<Result<usize, PipeError>>,
    /// The buffers of a transfer asked for (physical address, length).
    frames: Vec<(u64, u32), Kmalloc>,
    /// A control transfer's data, in or out.
    control: [u8; 8],
    /// The stick is gone: everything asked fails.
    gone: bool,
}

/// What a stick's thread and its controller's thread share.
pub struct Link {
    /// Where it is, `1-3.2`, for the log.
    pub place: TextBuf<24>,
    /// Its interface's number.
    pub interface: u8,
    /// How fast it is.
    pub speed: Speed,
    /// Its device's serial number, if it gives one.
    pub serial: TextBuf<SERIAL>,
    /// The device context indexes of its bulk IN and OUT endpoints.
    bulk_in: u8,
    bulk_out: u8,
    mail: SpinLock<Mail>,
    /// The controller's thread, which an ask wakes.
    controller: ThreadRef,
    /// The stick's thread, which an answer wakes, once it runs.
    stick: SpinLock<Option<ThreadRef>>,
    /// The transfer waited for is to be stopped: the disk's queue gave up
    /// on the request in flight ([`Link::abort`]).
    aborted: AtomicBool,
    /// Its place in line for names ([`wait_for_turn`]).
    ticket: u64,
}

/// A shared [`Link`].
pub type LinkRef = Arc<Link, Kmalloc>;

/// The sticks whose threads are starting, or run: each thread's link.
static STICKS: SpinLock<[Option<LinkRef>; MAX_STICKS]> =
    SpinLock::new([const { None }; MAX_STICKS]);
/// Sticks whose threads have not yet found their disks.
pub(super) static PROBING: AtomicUsize = AtomicUsize::new(0);
/// The next stick's place in line for names, and the first in line: at
/// boot, the sticks name their disks in the order their controllers
/// found them, so that the names are the same each boot.
static TICKETS: AtomicU64 = AtomicU64::new(0);
static TURN: AtomicU64 = AtomicU64::new(0);
/// The end of boot is past: sticks name their disks as they find them.
pub(super) static BOOTED: AtomicBool = AtomicBool::new(false);

/// Waits until the sticks found before the stick `link` at boot have
/// named their disks, or found that they have none.
pub fn wait_for_turn(link: &Link) {
    while TURN.load(Ordering::Acquire) < link.ticket && !BOOTED.load(Ordering::Acquire) {
        sched::sleep_ns(10_000_000);
    }
}

/// The stick `link`'s thread has found its disks, or that it has none:
/// the next in line names its own.
pub fn probed(link: &Link) {
    TURN.fetch_max(link.ticket + 1, Ordering::AcqRel);
    PROBING.fetch_sub(1, Ordering::AcqRel);
}

impl Link {
    /// Wakes the stick's thread, if it runs.
    fn wake_stick(&self) {
        let stick = self.stick.lock_irq().clone();
        if let Some(thread) = stick {
            sched::wake(&thread);
        }
    }

    /// The stick is gone (unplugged, or its controller failed): fails what
    /// was asked and everything after, and wakes the stick's thread.
    pub(super) fn gone(&self) {
        {
            let mut mail = self.mail.lock_irq();
            mail.gone = true;
            mail.ask = None;
            mail.started = None;
            mail.answer = Some(Err(PipeError::Gone));
        }
        self.wake_stick();
    }

    /// Whether the stick is gone.
    pub fn is_gone(&self) -> bool {
        self.mail.lock_irq().gone
    }

    /// Has the transfer the stick's thread waits for stopped: the disk's
    /// queue gave up waiting for its request.
    pub fn abort(&self) {
        self.aborted.store(true, Ordering::Release);
        self.wake_stick();
    }

    /// Forgets an [`Link::abort`] from before the command that starts.
    pub fn forget_abort(&self) {
        self.aborted.store(false, Ordering::Release);
    }
}

/// The thread of the stick whose link is `STICKS[index]`.
fn stick_main(index: usize) {
    let Some(link) = STICKS.lock_irq()[index].clone() else { return };
    *link.stick.lock_irq() = sched::current();
    crate::fs::disk::usb::run(&link);
    let link = STICKS.lock_irq()[index].take();
    drop(link);
}

/// How an error of the controller's driver reads as a pipe's.
fn pipe_error(err: radix_xhci::Error) -> PipeError {
    match err {
        radix_xhci::Error::Completion(code::STALL) => PipeError::Stall(0),
        radix_xhci::Error::Gone => PipeError::Gone,
        radix_xhci::Error::TimedOut => PipeError::TimedOut,
        _ => PipeError::Failed,
    }
}

impl Controller {
    /// Starts the mass storage interface `plan` of the configured device
    /// in slot `slot`: its thread, which finds its disks.
    #[inline(never)]
    pub(super) fn start_storage(&mut self, slot: u8, plan: StoragePlan) -> Result<(), Why> {
        let at = self.find(slot).ok_or(Why::Device("it is gone"))?;
        let (path, speed) = (self.devices[at].path, self.devices[at].device.place().speed);
        let interface = plan.interface;
        let (SUBCLASS_SCSI, PROTOCOL_BULK_ONLY) = (plan.subclass, plan.protocol) else {
            let what = match plan.protocol {
                PROTOCOL_UAS => "it speaks USB Attached SCSI alone, which is not supported yet",
                _ => "its kind of mass storage is not supported",
            };
            info!(
                "USB: {}: interface {interface} (subclass {:#04x}, protocol {:#04x}): {what}",
                Name(self.bus, path),
                plan.subclass,
                plan.protocol
            );
            return Ok(());
        };
        let (Some(bulk_in), Some(bulk_out)) = (plan.bulk_in, plan.bulk_out) else {
            return Err(Why::Device("its mass storage interface lacks a bulk pipe"));
        };
        if self.xhci.below_4g() {
            // Its transfers' buffers would need to be in the first 4 GiB,
            // which page caches are not kept to.
            return Err(Why::Device(
                "its controller reaches only the first 4 GiB of memory, which disks need more of",
            ));
        }
        let heap = HEAP.with(Flags::MAY_SLEEP);
        let mut frames = Vec::new_in(heap);
        frames.try_reserve_exact(MAX_TD_BUFFERS).map_err(|_| Why::Device("out of memory"))?;
        let controller = sched::current().ok_or(Why::Device("no thread"))?;
        let mut place = TextBuf::new();
        let _ = write!(place, "{}", Name(self.bus, path));
        let mut serial = TextBuf::new();
        if plan.serial != 0 {
            let Controller { xhci, host, buffer, devices, .. } = self;
            let setup = Setup::get_descriptor(descriptor::STRING, plan.serial, plan.language, 255);
            if let Ok(came) = devices[at].device.control_in(xhci, host, setup, &mut buffer[..255]) {
                let _ = radix_usb::string(&buffer[..came], &mut serial);
            }
        }
        let link = Link {
            place,
            interface,
            speed,
            serial,
            bulk_in: endpoint_index(bulk_in),
            bulk_out: endpoint_index(bulk_out),
            mail: SpinLock::new(Mail {
                ask: None,
                started: None,
                answer: None,
                frames,
                control: [0; 8],
                gone: false,
            }),
            controller,
            stick: SpinLock::new(None),
            aborted: AtomicBool::new(false),
            ticket: TICKETS.fetch_add(1, Ordering::AcqRel),
        };
        let link = Arc::try_new_in(link, heap).map_err(|_| Why::Device("out of memory"))?;
        let index = {
            let mut sticks = STICKS.lock_irq();
            let free = sticks.iter().position(Option::is_none);
            if let Some(index) = free {
                sticks[index] = Some(link.clone());
            }
            free
        };
        let index = index.ok_or(Why::Device("too many USB disks are plugged in"))?;
        let options = Options::new().held().policy(Policy::Fair { nice: -5 });
        match sched::spawn("usb-storage", stick_main, index, options) {
            Ok(thread) => {
                self.devices[at].storage = Some(link);
                PROBING.fetch_add(1, Ordering::AcqRel);
                sched::release(&thread);
                Ok(())
            }
            Err(_) => {
                let link = STICKS.lock_irq()[index].take();
                drop(link);
                Err(Why::Device("its thread could not be started"))
            }
        }
    }

    /// Carries out what the sticks' threads asked; says whether any did.
    pub(super) fn storage(&mut self) -> bool {
        let mut any = false;
        for at in 0..self.devices.len() {
            let Some(link) = self.devices[at].storage.clone() else { continue };
            let ask = link.mail.lock_irq().ask.take();
            let Some(ask) = ask else { continue };
            any = true;
            self.carry(at, &link, ask);
        }
        any
    }

    /// Carries out `ask` for the stick at `at`, whose link is `link`.
    #[inline(never)]
    fn carry(&mut self, at: usize, link: &Link, ask: Ask) {
        let Controller { xhci, host, devices, .. } = self;
        let device = &mut devices[at].device;
        let answer = match ask {
            Ask::Bulk(index) => {
                let mut mail = link.mail.lock_irq();
                match device.submit_frames(xhci, index, &mail.frames) {
                    Ok(td) => {
                        // Answered when its event comes.
                        mail.started = Some((index, td));
                        return;
                    }
                    Err(err) => Err(pipe_error(err)),
                }
            }
            Ask::ClearHalt(index) => {
                device.clear_halt(xhci, host, index).map(|()| 0).map_err(pipe_error)
            }
            Ask::Control(setup) => {
                let mut data = link.mail.lock_irq().control;
                let length = usize::from(setup.length).min(data.len());
                let result = if setup.device_to_host() {
                    device.control_in(xhci, host, setup, &mut data[..length])
                } else {
                    device.control_out(xhci, host, setup, &data[..length]).map(|()| 0)
                };
                link.mail.lock_irq().control = data;
                result.map_err(pipe_error)
            }
            Ask::Cancel => {
                let started = link.mail.lock_irq().started.take();
                match started {
                    Some((index, _)) => {
                        // Stopped, and its ring moved past it.
                        let _ = device.recover(xhci, host, index, code::STOPPED);
                        Err(PipeError::TimedOut)
                    }
                    // It ended before it could be stopped, and said how.
                    None if link.mail.lock_irq().answer.is_some() => return,
                    // It was never started.
                    None => Err(PipeError::TimedOut),
                }
            }
        };
        {
            let mut mail = link.mail.lock_irq();
            if !mail.gone {
                mail.answer = Some(answer);
            }
        }
        link.wake_stick();
    }

    /// Whether the transfer at `trb` on endpoint `endpoint` of the device
    /// at `at`, which finished with `code`, `residual` bytes of its TRB
    /// short, is its stick's; hands back how it ended if so.
    pub(super) fn storage_transfer(
        &mut self,
        at: usize,
        trb: u64,
        endpoint: u8,
        code: u8,
        residual: u32,
    ) -> bool {
        let Some(link) = &self.devices[at].storage else { return false };
        {
            let mut mail = link.mail.lock_irq();
            let Some((index, td)) = mail.started else { return false };
            if index != endpoint || td.position(trb).is_none() {
                return false;
            }
            mail.started = None;
            let moved = td.moved(&mail.frames, trb, residual).unwrap_or(0);
            mail.answer = Some(match code {
                code::SUCCESS | code::SHORT_PACKET => Ok(moved),
                code::STALL => Err(PipeError::Stall(moved)),
                // Halted: the reset that follows brings it back.
                _ => Err(PipeError::Failed),
            });
        }
        link.wake_stick();
        true
    }
}

/// A stick's pipes, as its thread asks its controller's thread to carry
/// them.
pub struct StickPipes<'a> {
    link: &'a Link,
    /// A page for command blocks, statuses and the probe's answers.
    page: Dma,
}

impl<'a> StickPipes<'a> {
    /// The pipes of the stick whose link is `link`; `None` when memory
    /// runs out.
    pub fn new(link: &'a Link) -> Option<StickPipes<'a>> {
        let page = <crate::Arch as radix_hal::Devices>::alloc_dma(1, false)?;
        Some(StickPipes { link, page })
    }

    /// Asks the controller's thread for `ask` and waits for its answer:
    /// for a transfer, at most `timeout_ms`, or until the disk's queue
    /// gives up on it, after which it is stopped.
    fn ask(&self, ask: Ask, timeout_ms: Option<u32>) -> Result<usize, PipeError> {
        let link = self.link;
        {
            let mut mail = link.mail.lock_irq();
            if mail.gone {
                return Err(PipeError::Gone);
            }
            mail.answer = None;
            mail.ask = Some(ask);
        }
        sched::wake(&link.controller);
        let deadline =
            timeout_ms.map(|ms| sched::now_ns().saturating_add(u64::from(ms) * 1_000_000));
        let mut cancelled = false;
        loop {
            {
                let mut mail = link.mail.lock_irq();
                if let Some(answer) = mail.answer.take() {
                    return answer;
                }
                let late = deadline.is_some_and(|at| sched::now_ns() >= at)
                    || link.aborted.load(Ordering::Acquire);
                if let (Ask::Bulk(_), true, false) = (ask, late, cancelled) {
                    // Stopped, unless it ends first; either way, answered.
                    mail.ask = Some(Ask::Cancel);
                    cancelled = true;
                    drop(mail);
                    sched::wake(&link.controller);
                }
            }
            let until = match deadline {
                Some(at) if !cancelled => at,
                _ => sched::now_ns().saturating_add(LOOK_NS),
            };
            sched::block_until(until, "usb-storage");
        }
    }

    /// A transfer of `frames` on the bulk endpoint `index`.
    fn transfer(
        &self,
        index: u8,
        frames: &[(u64, u32)],
        timeout_ms: u32,
    ) -> Result<usize, PipeError> {
        if frames.is_empty() || frames.len() > MAX_TD_BUFFERS {
            return Err(PipeError::Failed);
        }
        {
            let mut mail = self.link.mail.lock_irq();
            mail.frames.clear();
            for &frame in frames {
                // Room for the most was made with the link.
                let _ = mail.frames.push_within_capacity(frame);
            }
        }
        self.ask(Ask::Bulk(index), Some(timeout_ms))
    }
}

impl Pipes for StickPipes<'_> {
    fn bulk(&mut self, data: Data<'_>, timeout_ms: u32) -> Result<usize, PipeError> {
        let link = self.link;
        let index = if data.into_host() { link.bulk_in } else { link.bulk_out };
        let page = self.page.phys();
        match data {
            Data::None => Ok(0),
            Data::In(buffer) => {
                if buffer.len() > PAGE_SIZE {
                    return Err(PipeError::Failed);
                }
                let result = self.transfer(index, &[(page, buffer.len() as u32)], timeout_ms);
                let came = match result {
                    Ok(came) | Err(PipeError::Stall(came)) => came.min(buffer.len()),
                    Err(_) => 0,
                };
                self.page.read(0, &mut buffer[..came]);
                result
            }
            Data::Out(bytes) => {
                if bytes.len() > PAGE_SIZE {
                    return Err(PipeError::Failed);
                }
                self.page.write(0, bytes);
                self.transfer(index, &[(page, bytes.len() as u32)], timeout_ms)
            }
            Data::Frames { frames, .. } => self.transfer(index, frames, timeout_ms),
        }
    }

    fn clear_halt(&mut self, into_host: bool) -> Result<(), PipeError> {
        let index = if into_host { self.link.bulk_in } else { self.link.bulk_out };
        self.ask(Ask::ClearHalt(index), None).map(|_| ())
    }

    fn control(&mut self, setup: Setup, data: Data<'_>) -> Result<usize, PipeError> {
        let length = usize::from(setup.length);
        if length > 8 {
            return Err(PipeError::Failed);
        }
        if let Data::Out(bytes) = &data {
            let mut control = [0u8; 8];
            let n = bytes.len().min(length);
            control[..n].copy_from_slice(&bytes[..n]);
            self.link.mail.lock_irq().control = control;
        }
        let came = self.ask(Ask::Control(setup), None)?;
        if let Data::In(buffer) = data {
            let control = self.link.mail.lock_irq().control;
            let n = came.min(buffer.len()).min(control.len());
            buffer[..n].copy_from_slice(&control[..n]);
        }
        Ok(came)
    }
}
