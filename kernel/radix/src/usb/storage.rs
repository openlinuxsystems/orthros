// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! USB sticks, card readers and USB disks: mass storage interfaces of the
//! Bulk-Only Transport (Phase 11b, ADR 0161) and of USB Attached SCSI
//! (UAS, ADR 0162), as the controller's thread carries them.
//!
//! Once a device is configured, [`Controller::start_storage`] starts a
//! kernel thread for its mass storage interface, `usb-storage`, as Linux
//! has one: it finds the disks behind the interface and serves their
//! requests (`fs::disk::usb`). Only the controller's thread drives the
//! controller, so the stick's thread asks it for each transfer
//! ([`Link`]), straight to or from the buffers it names
//! (`radix_xhci::Device::submit_stream`), which the controller's thread
//! starts, and whose event hands back how much moved
//! ([`Controller::storage_transfer`]). The controller's thread looks for
//! what was asked each time it wakes, and the stick's thread wakes it for
//! each ask. A device unplugged, or a controller that fails, fails what
//! was asked and every ask after ([`Link::gone`]), which ends the stick's
//! thread and takes its disks away.
//!
//! A Bulk-Only interface's commands go one at a time, in stages, a
//! transfer each, which the stick's thread waits for ([`StickPipes`]): a
//! transfer on a bulk pipe; a pipe's halt cleared; a control transfer
//! (Get Max LUN, the class's reset); or a transfer that took too long
//! stopped. A UAS interface has many commands in flight, so its thread
//! asks for transfers in order without waiting, and takes each as it ends
//! ([`UasPipes`]): a Command IU, a status or a command's data started on
//! its pipe (and, at SuperSpeed, on the stream its tag names), a data
//! transfer stopped, or the device reset. A device is preferred as UAS,
//! as Linux prefers it, if the controller has the streams it needs at
//! SuperSpeed: its UAS setting is chosen (SET_INTERFACE, with Configure
//! Endpoint for its endpoints) and its status and data pipes are given
//! as many streams as they, the controller and Linux's 256 take, at most
//! [`radix_xhci::ring::MAX_STREAMS`]. A device reset is a port reset, then
//! the slot reset and its endpoints, streams, configuration and setting
//! set up again, as Linux's `usb_reset_device` does.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use radix_hw::{Dma, PAGE_SIZE};
use radix_klog::{TextBuf, info};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_scsi::Data;
use radix_sync::SpinLock;
use radix_usb::storage::uas::{self, Action, COMMAND_LENGTH, MAX_TAGS, Pipe, STATUS_LENGTH};
use radix_usb::storage::{PROTOCOL_BULK_ONLY, PROTOCOL_UAS, PipeError, Pipes, SUBCLASS_SCSI};
use radix_usb::{EndpointDescriptor, Interface, Setup, Speed, TransferType, class, descriptor};
use radix_xhci::{Event, MAX_STREAM_TD_BUFFERS, MAX_TD_BUFFERS, Td, code, endpoint_index};

use super::controller::{Controller, Name, Path, RECOVERY_NS, Why, alloc};
use crate::fs::disk::SERIAL;
use crate::sched::{self, Options, Policy, ThreadRef};

/// The most sticks driven at once.
const MAX_STICKS: usize = 16;
/// How often a stick's thread that waits for its controller's looks
/// whether the stick is gone, at most.
const LOOK_NS: u64 = 1_000_000_000;
/// The most endpoints of setting 0 that another setting replaces.
const MAX_SETTING_ENDPOINTS: usize = 4;
/// Where each tag's status lands in a UAS stick's status page.
const STATUS_STRIDE: usize = 128;
/// The streams a UAS interface asks for: Linux's 256 (`MAX_CMNDS`).
const WANTED_STREAMS: usize = 256;

/// A mass storage interface in a device's configuration, and the settings
/// it has that the kernel speaks.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct StoragePlan {
    /// bInterfaceNumber.
    pub(super) interface: u8,
    /// Setting 0's subclass and protocol, which say how its commands go.
    subclass: u8,
    protocol: u8,
    /// Setting 0's bulk IN and OUT endpoints' addresses, if it speaks the
    /// Bulk-Only Transport.
    bot: Option<(u8, u8)>,
    /// A UAS setting: its number and its pipes' endpoints.
    uas: Option<(u8, [EndpointDescriptor; 4])>,
    /// Setting 0's endpoints' addresses, the first `setting0_count`: those
    /// another setting's take the place of.
    setting0: [u8; MAX_SETTING_ENDPOINTS],
    setting0_count: usize,
    /// The configuration's value, which a reset sets again.
    pub(super) configuration: u8,
    /// Its device's serial number string, and the language its strings
    /// are read in.
    serial: u8,
    language: u16,
}

impl StoragePlan {
    /// The mass storage interface whose settings are `settings`, setting
    /// 0 first, in configuration `configuration` of a device whose serial
    /// number is string `serial`, its strings read in `language`.
    pub(super) fn of<'a>(
        mut settings: impl Iterator<Item = Interface<'a>>,
        configuration: u8,
        (serial, language): (u8, u16),
    ) -> Option<StoragePlan> {
        let first = settings.next()?;
        let d = first.descriptor;
        let mut plan = StoragePlan {
            interface: d.number,
            subclass: d.subclass,
            protocol: d.protocol,
            configuration,
            serial,
            language,
            ..StoragePlan::default()
        };
        for endpoint in first.endpoints() {
            if plan.setting0_count < MAX_SETTING_ENDPOINTS {
                plan.setting0[plan.setting0_count] = endpoint.address;
                plan.setting0_count += 1;
            }
        }
        if (d.subclass, d.protocol) == (SUBCLASS_SCSI, PROTOCOL_BULK_ONLY) {
            let (mut bulk_in, mut bulk_out) = (None, None);
            for endpoint in first.endpoints().filter(|e| e.transfer_type() == TransferType::Bulk) {
                let pipe = if endpoint.is_in() { &mut bulk_in } else { &mut bulk_out };
                pipe.get_or_insert(endpoint.address);
            }
            plan.bot = bulk_in.zip(bulk_out);
        }
        // The UAS setting, as Linux's uas_find_uas_alt_setting finds it.
        let uas = |s: &Interface<'_>| {
            let d = s.descriptor;
            (d.class, d.subclass, d.protocol) == (class::MASS_STORAGE, SUBCLASS_SCSI, PROTOCOL_UAS)
        };
        plan.uas = core::iter::once(first)
            .chain(settings)
            .filter(uas)
            .find_map(|s| Some((s.descriptor.alternate, uas::find_pipes(&s)?)));
        Some(plan)
    }
}

/// How a stick's commands go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// The Bulk-Only Transport: the device context indexes of its bulk IN
    /// and OUT endpoints.
    Bot {
        /// The bulk IN endpoint's.
        bulk_in: u8,
        /// The bulk OUT endpoint's.
        bulk_out: u8,
    },
    /// USB Attached SCSI: its pipes' device context indexes, in
    /// [`Pipe::ALL`]'s order, and the streams its status and data pipes
    /// have, if they have any.
    Uas {
        /// The pipes' indexes.
        pipes: [u8; 4],
        /// Their streams.
        streams: Option<usize>,
    },
}

/// What a Bulk-Only stick's thread asks of its controller's thread.
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

/// What passes between a Bulk-Only stick's thread and its controller's.
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

/// A transfer a UAS stick's thread asks for: what [`uas::Uas`] said to do,
/// and the one buffer it moves, or none for its command's data.
type UasAsk = (Action, Option<(u64, u32)>);

/// A transfer on a UAS stick's pipe that the controller's thread started:
/// where it is, and its one buffer, or none for its command's data.
type UasStarted = (Td, Option<(u64, u32)>);

/// What passes between a UAS stick's thread and its controller's.
struct UasMail {
    /// What was asked, in order.
    asks: Vec<UasAsk, Kmalloc>,
    /// The device is to be reset ([`UasMail::reset_answer`] says how it
    /// went).
    reset: bool,
    reset_answer: Option<Result<(), PipeError>>,
    /// The transfers started, by pipe, then by number less one.
    started: Vec<Option<UasStarted>, Kmalloc>,
    /// The transfers that ended, in the order they did, with the bytes
    /// that moved.
    ended: Vec<(Pipe, u16, Result<usize, PipeError>), Kmalloc>,
    /// The buffers of each tag's data, by tag less one.
    data: Vec<Vec<(u64, u32), Kmalloc>, Kmalloc>,
}

impl UasMail {
    fn at(pipe: Pipe, n: u16) -> usize {
        pipe.index() * MAX_TAGS + usize::from(n) - 1
    }
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
    /// How its commands go.
    pub protocol: Protocol,
    /// Its interface's setting, and its device's configuration, which a
    /// reset sets again.
    alternate: u8,
    configuration: u8,
    mail: SpinLock<Mail>,
    uas: SpinLock<UasMail>,
    /// The controller's thread, which an ask wakes.
    controller: ThreadRef,
    /// The controller's place on PCI, which an IOMMU knows its DMA by.
    pub source: radix_hw::pci::Address,
    /// The stick's thread, which an answer wakes, once it runs.
    stick: SpinLock<Option<ThreadRef>>,
    /// The transfer waited for is to be stopped: the disk's queue gave up
    /// on a request in flight ([`Link::abort`]).
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
        {
            let mut mail = self.uas.lock_irq();
            mail.asks.clear();
            mail.started.fill(None);
            mail.reset_answer = Some(Err(PipeError::Gone));
        }
        self.wake_stick();
    }

    /// Whether the stick is gone.
    pub fn is_gone(&self) -> bool {
        self.mail.lock_irq().gone
    }

    /// Has the transfer the stick's thread waits for stopped, or for a
    /// UAS stick, the device reset: the disk's queue gave up waiting for
    /// its request.
    pub fn abort(&self) {
        self.aborted.store(true, Ordering::Release);
        self.wake_stick();
    }

    /// Forgets an [`Link::abort`] from before the command that starts.
    pub fn forget_abort(&self) {
        self.aborted.store(false, Ordering::Release);
    }

    /// Takes an [`Link::abort`]: whether one came since the last.
    pub fn take_abort(&self) -> bool {
        self.aborted.swap(false, Ordering::AcqRel)
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

/// Whether a transfer event says a Stop Endpoint stopped its TRB.
fn stopped(code: u8) -> bool {
    matches!(code, code::STOPPED | code::STOPPED_LENGTH_INVALID | code::STOPPED_SHORT_PACKET)
}

/// Memory for a stick's mail: `None` when it runs out.
fn mail(protocol: Protocol) -> Option<(Mail, UasMail)> {
    let heap = HEAP.with(Flags::MAY_SLEEP);
    let mut frames = Vec::new_in(heap);
    frames.try_reserve_exact(MAX_TD_BUFFERS).ok()?;
    let mail =
        Mail { ask: None, started: None, answer: None, frames, control: [0; 8], gone: false };
    let mut uas = UasMail {
        asks: Vec::new_in(heap),
        reset: false,
        reset_answer: None,
        started: Vec::new_in(heap),
        ended: Vec::new_in(heap),
        data: Vec::new_in(heap),
    };
    if let Protocol::Uas { .. } = protocol {
        // Each tag at most starts its status, data and command, and stops
        // its data, at once.
        uas.asks.try_reserve_exact(5 * MAX_TAGS).ok()?;
        uas.started.try_reserve_exact(4 * MAX_TAGS).ok()?;
        uas.ended.try_reserve_exact(4 * MAX_TAGS).ok()?;
        uas.data.try_reserve_exact(MAX_TAGS).ok()?;
        while uas.started.push_within_capacity(None).is_ok() {}
        for _ in 0..MAX_TAGS {
            let mut frames = Vec::new_in(heap);
            frames.try_reserve_exact(MAX_STREAM_TD_BUFFERS).ok()?;
            let _ = uas.data.push_within_capacity(frames);
        }
    }
    Some((mail, uas))
}

impl Controller {
    /// Starts the mass storage interface `plan` of the configured device
    /// in slot `slot`: its thread, which finds its disks.
    #[inline(never)]
    pub(super) fn start_storage(&mut self, slot: u8, plan: StoragePlan) -> Result<(), Why> {
        let at = self.find(slot).ok_or(Why::Device("it is gone"))?;
        let (path, speed) = (self.devices[at].path, self.devices[at].device.place().speed);
        let Some((protocol, alternate)) = self.storage_protocol(at, &plan)? else {
            return Ok(());
        };
        if self.xhci.below_4g() {
            // Its transfers' buffers would need to be in the first 4 GiB,
            // which page caches are not kept to.
            return Err(Why::Device(
                "its controller reaches only the first 4 GiB of memory, which disks need more of",
            ));
        }
        let (mail, uas) = mail(protocol).ok_or(Why::Device("out of memory"))?;
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
            interface: plan.interface,
            speed,
            serial,
            protocol,
            alternate,
            configuration: plan.configuration,
            mail: SpinLock::new(mail),
            uas: SpinLock::new(uas),
            controller,
            source: self.source,
            stick: SpinLock::new(None),
            aborted: AtomicBool::new(false),
            ticket: TICKETS.fetch_add(1, Ordering::AcqRel),
        };
        let heap = HEAP.with(Flags::MAY_SLEEP);
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

    /// Chooses how the commands of the interface `plan` of the device at
    /// `at` go, as Linux's `uas_use_uas_driver` does, and sets it up: UAS
    /// if it has a UAS setting, and at SuperSpeed the controller has the
    /// streams it needs, or else the Bulk-Only Transport if setting 0
    /// speaks it. Returns how, and the setting chosen; `None` if neither
    /// is spoken, which the log says.
    #[inline(never)]
    fn storage_protocol(
        &mut self,
        at: usize,
        plan: &StoragePlan,
    ) -> Result<Option<(Protocol, u8)>, Why> {
        let (path, speed) = (self.devices[at].path, self.devices[at].device.place().speed);
        let interface = plan.interface;
        let streams = self.xhci.info().stream_contexts > 0;
        let uas = plan.uas.filter(|(_, pipes)| {
            !speed.is_super() || (streams && pipes[1..].iter().all(|e| e.max_streams() > 0))
        });
        if let Some((alternate, pipes)) = uas {
            return self.set_up_uas(at, plan, alternate, &pipes).map(Some);
        }
        let what = match (plan.bot, plan.uas) {
            (Some((bulk_in, bulk_out)), uas) => {
                if uas.is_some() {
                    info!(
                        "USB: {}: interface {interface}: the controller has no streams, which USB Attached SCSI needs; the Bulk-Only Transport is used",
                        Name(self.bus, path)
                    );
                }
                let (bulk_in, bulk_out) = (endpoint_index(bulk_in), endpoint_index(bulk_out));
                return Ok(Some((Protocol::Bot { bulk_in, bulk_out }, 0)));
            }
            (None, Some(_)) => {
                "it speaks USB Attached SCSI alone, which needs streams this controller lacks"
            }
            (None, None)
                if (plan.subclass, plan.protocol) == (SUBCLASS_SCSI, PROTOCOL_BULK_ONLY) =>
            {
                return Err(Why::Device("its mass storage interface lacks a bulk pipe"));
            }
            (None, None) => "its kind of mass storage is not supported",
        };
        info!(
            "USB: {}: interface {interface} (subclass {:#04x}, protocol {:#04x}): {what}",
            Name(self.bus, path),
            plan.subclass,
            plan.protocol
        );
        Ok(None)
    }

    /// Puts the interface `plan` of the device at `at` in its UAS setting
    /// `alternate`, whose pipes are `pipes`, and gives its status and data
    /// pipes their streams at SuperSpeed.
    #[inline(never)]
    fn set_up_uas(
        &mut self,
        at: usize,
        plan: &StoragePlan,
        alternate: u8,
        pipes: &[EndpointDescriptor; 4],
    ) -> Result<(Protocol, u8), Why> {
        let Controller { xhci, host, devices, source, .. } = self;
        let mut alloc = alloc(*source);
        let device = &mut devices[at].device;
        if alternate != 0 {
            let mut drop = [0u8; MAX_SETTING_ENDPOINTS];
            for (to, &address) in drop.iter_mut().zip(&plan.setting0[..plan.setting0_count]) {
                *to = endpoint_index(address);
            }
            let setting = (plan.interface, alternate);
            device.set_interface(
                xhci,
                host,
                &mut alloc,
                setting,
                &drop[..plan.setting0_count],
                pipes,
            )?;
        }
        let indexes = pipes.map(|e| endpoint_index(e.address));
        let streams = if device.place().speed.is_super() {
            Some(device.alloc_streams(xhci, host, &mut alloc, &indexes[1..], WANTED_STREAMS)?)
        } else {
            None
        };
        Ok((Protocol::Uas { pipes: indexes, streams }, alternate))
    }

    /// Carries out what the sticks' threads asked; says whether any did.
    pub(super) fn storage(&mut self) -> bool {
        let mut any = false;
        for at in 0..self.devices.len() {
            let Some(link) = self.devices[at].storage.clone() else { continue };
            if let Protocol::Uas { .. } = link.protocol {
                any |= self.carry_uas(at, &link);
                continue;
            }
            let ask = link.mail.lock_irq().ask.take();
            let Some(ask) = ask else { continue };
            any = true;
            self.carry(at, &link, ask);
        }
        any
    }

    /// Carries out `ask` for the Bulk-Only stick at `at`, whose link is
    /// `link`.
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

    /// Carries out what the UAS stick at `at`, whose link is `link`,
    /// asked, in order; says whether it asked anything.
    #[inline(never)]
    fn carry_uas(&mut self, at: usize, link: &Link) -> bool {
        let mut any = false;
        loop {
            let (reset, ask) = {
                let mut mail = link.uas.lock_irq();
                let reset = core::mem::take(&mut mail.reset);
                let ask =
                    if reset || mail.asks.is_empty() { None } else { Some(mail.asks.remove(0)) };
                (reset, ask)
            };
            if reset {
                let result = self.reset_storage(at, link);
                {
                    let mut mail = link.uas.lock_irq();
                    // Every transfer is gone with the reset, and what was
                    // asked before it is not carried out.
                    mail.asks.clear();
                    mail.started.fill(None);
                    mail.ended.clear();
                    mail.reset_answer = Some(result);
                }
                link.wake_stick();
                return true;
            }
            let Some((action, frame)) = ask else { return any };
            any = true;
            match action {
                Action::Start(pipe, n) => self.start_uas(at, link, (pipe, n), frame),
                Action::Cancel(pipe, n) => self.cancel_uas(at, link, (pipe, n)),
            }
        }
    }

    /// Starts transfer `n` on the UAS stick's pipe `pipe`, of `frame`, or
    /// of its command's data.
    fn start_uas(
        &mut self,
        at: usize,
        link: &Link,
        (pipe, n): (Pipe, u16),
        frame: Option<(u64, u32)>,
    ) {
        let Protocol::Uas { pipes, streams } = link.protocol else { return };
        if n == 0 || usize::from(n) > MAX_TAGS {
            return;
        }
        let index = pipes[pipe.index()];
        let stream = if streams.is_some() && pipe != Pipe::Command { usize::from(n) } else { 0 };
        let Controller { xhci, devices, .. } = self;
        let device = &mut devices[at].device;
        {
            let mut mail = link.uas.lock_irq();
            let result = match &frame {
                Some(frame) => {
                    device.submit_stream(xhci, index, stream, core::slice::from_ref(frame))
                }
                None => device.submit_stream(xhci, index, stream, &mail.data[usize::from(n) - 1]),
            };
            match result {
                // Answered when its event comes.
                Ok(td) => {
                    mail.started[UasMail::at(pipe, n)] = Some((td, frame));
                    return;
                }
                Err(err) => {
                    let _ = mail.ended.push_within_capacity((pipe, n, Err(pipe_error(err))));
                }
            }
        }
        link.wake_stick();
    }

    /// Stops transfer `n` on the UAS stick's pipe `pipe`, if it has not
    /// ended: on its stream, the others on the pipe going on; without
    /// streams, the pipe's ring skips every transfer on it, and those
    /// others fail.
    fn cancel_uas(&mut self, at: usize, link: &Link, (pipe, n): (Pipe, u16)) {
        let Protocol::Uas { pipes, streams } = link.protocol else { return };
        if n == 0 || usize::from(n) > MAX_TAGS {
            return;
        }
        let index = pipes[pipe.index()];
        // Which transfers are on the pipe, a bit each.
        let (started, busy) = {
            let mut mail = link.uas.lock_irq();
            let started = mail.started[UasMail::at(pipe, n)].take();
            let busy = (1..=MAX_TAGS as u16)
                .filter(|&m| mail.started[UasMail::at(pipe, m)].is_some())
                .fold(0u32, |bits, m| bits | 1 << (m - 1));
            (started, busy)
        };
        if started.is_none() {
            // It ended, and said so.
            return;
        }
        let Controller { xhci, host, devices, .. } = self;
        let device = &mut devices[at].device;
        let result = if streams.is_some() && pipe != Pipe::Command {
            let pending = |stream: usize| busy & (1 << (stream - 1)) != 0;
            device.cancel_stream(xhci, host, index, usize::from(n), &pending)
        } else {
            device.recover(xhci, host, index, code::STOPPED)
        };
        {
            let mut mail = link.uas.lock_irq();
            let answer = match result {
                Err(radix_xhci::Error::Gone) => Err(PipeError::Gone),
                _ => Err(PipeError::TimedOut),
            };
            let _ = mail.ended.push_within_capacity((pipe, n, answer));
            if streams.is_none() || pipe == Pipe::Command {
                for m in 1..=MAX_TAGS as u16 {
                    if mail.started[UasMail::at(pipe, m)].take().is_some() {
                        let _ = mail.ended.push_within_capacity((pipe, m, Err(PipeError::Failed)));
                    }
                }
            }
        }
        link.wake_stick();
    }

    /// Resets the UAS stick at `at`, whose link is `link`, as Linux's
    /// `usb_reset_device` does: its port, then its slot, endpoints,
    /// streams, configuration and setting. Every transfer it had is gone;
    /// the events they left are dropped.
    #[inline(never)]
    fn reset_storage(&mut self, at: usize, link: &Link) -> Result<(), PipeError> {
        let (path, slot) = (self.devices[at].path, self.devices[at].device.slot());
        let reset = if path.route == 0 {
            self.reset_port(path.root_port, false)
        } else {
            let shift = 4 * (path.depth() - 1);
            let port = ((path.route >> shift) & 0xf) as u8;
            let parent = Path { root_port: path.root_port, route: path.route & !(0xf << shift) };
            let hub = self.devices.iter().find(|d| d.path == parent);
            match hub.and_then(|d| Some((d.device.slot(), d.hub.as_ref()?.usb3))) {
                Some((hub, usb3)) => self.hub_reset(hub, port, usb3).map(|_| ()),
                None => Err(Why::Device("its hub is gone")),
            }
        };
        let failed = |why: Why| match why {
            Why::Device("it was unplugged") | Why::Driver(radix_xhci::Error::Gone) => {
                PipeError::Gone
            }
            _ => PipeError::Failed,
        };
        reset.map_err(failed)?;
        sched::sleep_ns(RECOVERY_NS);
        let at = self.find(slot).ok_or(PipeError::Gone)?;
        let Controller { xhci, host, devices, .. } = self;
        let device = &mut devices[at].device;
        let result = device.reset(xhci, host, link.configuration).and_then(|()| {
            if link.alternate == 0 {
                return Ok(());
            }
            let setup = Setup::set_interface(link.interface, link.alternate);
            device.control_out(xhci, host, setup, &[])
        });
        // Events of the transfers the reset ended: their TRBs may be
        // those of transfers started after.
        self.host.kept.retain(|event| {
            !matches!(event, Event::Transfer { slot: s, endpoint, .. } if *s == slot && *endpoint != 1)
        });
        result.map_err(|err| failed(Why::Driver(err)))
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
        if let Protocol::Uas { pipes, .. } = link.protocol {
            return uas_transfer(link, pipes, (trb, endpoint), code, residual);
        }
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

/// Whether the transfer at `trb` on endpoint `endpoint`, which finished
/// with `code`, is one of the UAS stick `link`'s, whose pipes' indexes are
/// `pipes`, and hands it back to the stick's thread if it is. One that a Stop Endpoint
/// stopped goes on when its stream's doorbell rings again, or was stopped
/// for good, which [`Controller::cancel_uas`] answers.
fn uas_transfer(
    link: &Link,
    pipes: [u8; 4],
    (trb, endpoint): (u64, u8),
    code: u8,
    residual: u32,
) -> bool {
    let Some(pipe) = Pipe::ALL.into_iter().find(|p| pipes[p.index()] == endpoint) else {
        return false;
    };
    {
        let mut mail = link.uas.lock_irq();
        let found = (1..=MAX_TAGS as u16).find(|&n| {
            mail.started[UasMail::at(pipe, n)].is_some_and(|(td, _)| td.position(trb).is_some())
        });
        let Some(n) = found else { return false };
        if stopped(code) {
            return true;
        }
        let Some((td, frame)) = mail.started[UasMail::at(pipe, n)].take() else { return false };
        let moved = match &frame {
            Some(frame) => td.moved(core::slice::from_ref(frame), trb, residual),
            None => td.moved(&mail.data[usize::from(n) - 1], trb, residual),
        };
        let moved = moved.unwrap_or(0);
        let answer = match code {
            code::SUCCESS | code::SHORT_PACKET => Ok(moved),
            code::STALL => Err(PipeError::Stall(moved)),
            _ => Err(PipeError::Failed),
        };
        let _ = mail.ended.push_within_capacity((pipe, n, answer));
    }
    link.wake_stick();
    true
}

/// A Bulk-Only stick's pipes, as its thread asks its controller's thread
/// to carry them.
pub struct StickPipes<'a> {
    link: &'a Link,
    /// A page for command blocks, statuses and the probe's answers.
    page: Dma,
}

impl<'a> StickPipes<'a> {
    /// The pipes of the stick whose link is `link`; `None` when memory
    /// runs out.
    pub fn new(link: &'a Link) -> Option<StickPipes<'a>> {
        let page = <crate::Arch as radix_hal::Devices>::alloc_dma(link.source, 1, false)?;
        Some(StickPipes { link, page })
    }

    /// The bulk endpoints' indexes, IN then OUT.
    fn endpoints(&self) -> (u8, u8) {
        match self.link.protocol {
            Protocol::Bot { bulk_in, bulk_out } => (bulk_in, bulk_out),
            Protocol::Uas { .. } => (0, 0),
        }
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
        let (bulk_in, bulk_out) = self.endpoints();
        let index = if data.into_host() { bulk_in } else { bulk_out };
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
        let (bulk_in, bulk_out) = self.endpoints();
        let index = if into_host { bulk_in } else { bulk_out };
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

/// A UAS stick's pipes, as its thread asks its controller's thread to
/// carry them: in order, without waiting, each transfer taken as it ends.
pub struct UasPipes<'a> {
    link: &'a Link,
    /// Each tag's Command IU, [`COMMAND_LENGTH`] bytes each.
    ius: Dma,
    /// Each status buffer's IU, [`STATUS_STRIDE`] bytes each.
    statuses: Dma,
    /// Data to or from a buffer of the kernel's own, not a request's: the
    /// probe's, the partition table's.
    bounce: Dma,
}

impl<'a> UasPipes<'a> {
    /// The pipes of the UAS stick whose link is `link`; `None` when memory
    /// runs out.
    pub fn new(link: &'a Link) -> Option<UasPipes<'a>> {
        let page = || <crate::Arch as radix_hal::Devices>::alloc_dma(link.source, 1, false);
        Some(UasPipes { link, ius: page()?, statuses: page()?, bounce: page()? })
    }

    /// Asks for `action`, of `frame`, or of its command's data.
    fn ask(&self, action: Action, frame: Option<(u64, u32)>) {
        {
            let mut mail = self.link.uas.lock_irq();
            // As many as the commands in flight can ask at once.
            let _ = mail.asks.push_within_capacity((action, frame));
        }
        sched::wake(&self.link.controller);
    }

    /// Hands the controller's thread the buffers of the data of the
    /// command with tag `tag`, before it is asked for.
    pub fn set_data(&self, tag: u16, frames: &[(u64, u32)]) {
        let Some(at) = usize::from(tag).checked_sub(1).filter(|&at| at < MAX_TAGS) else { return };
        let mut mail = self.link.uas.lock_irq();
        let data = &mut mail.data[at];
        data.clear();
        for &frame in frames {
            // Room for a stream's TD was made with the link.
            let _ = data.push_within_capacity(frame);
        }
    }

    /// Puts the Command IU `iu` of the command with tag `tag` where its
    /// transfer moves it from.
    pub fn place_iu(&self, tag: u16, iu: &[u8; COMMAND_LENGTH]) {
        if (1..=MAX_TAGS as u16).contains(&tag) {
            self.ius.write((usize::from(tag) - 1) * COMMAND_LENGTH, iu);
        }
    }

    /// Asks for what `uas` says to do: its commands' IUs where
    /// [`UasPipes::place_iu`] put them, their data in the buffers
    /// [`UasPipes::set_data`] gave. Says whether it said anything.
    pub fn act(&self, uas: &mut uas::Uas) -> bool {
        let mut any = false;
        while let Some(action) = uas.next_action() {
            any = true;
            match action {
                Action::Start(Pipe::Command, n) => self.start_command(n),
                Action::Start(Pipe::Status, n) => self.start_status(n),
                Action::Start(_, _) | Action::Cancel(_, _) => self.ask(action, None),
            }
        }
        any
    }

    fn start_command(&self, n: u16) {
        let at = (usize::from(n) - 1) * COMMAND_LENGTH;
        let frame = (self.ius.phys() + at as u64, COMMAND_LENGTH as u32);
        self.ask(Action::Start(Pipe::Command, n), Some(frame));
    }

    fn start_status(&self, n: u16) {
        let at = (usize::from(n) - 1) * STATUS_STRIDE;
        let frame = (self.statuses.phys() + at as u64, STATUS_LENGTH as u32);
        self.ask(Action::Start(Pipe::Status, n), Some(frame));
    }

    /// The next transfer that ended: its pipe, its number, and the bytes
    /// that moved, a status's copied into `status`. A stick that is gone
    /// says so once.
    pub fn take_ended(
        &self,
        status: &mut [u8; STATUS_LENGTH],
    ) -> Option<(Pipe, u16, Result<usize, PipeError>)> {
        if self.link.is_gone() {
            return Some((Pipe::Command, 0, Err(PipeError::Gone)));
        }
        let ended = {
            let mut mail = self.link.uas.lock_irq();
            if mail.ended.is_empty() { None } else { Some(mail.ended.remove(0)) }
        }?;
        if let (Pipe::Status, n, Ok(came)) = ended {
            let at = (usize::from(n) - 1) * STATUS_STRIDE;
            self.statuses.read(at, &mut status[..came.min(STATUS_LENGTH)]);
        }
        Some(ended)
    }

    /// Resets the device, and waits until it is.
    pub fn reset(&self) -> Result<(), PipeError> {
        {
            let mut mail = self.link.uas.lock_irq();
            mail.reset_answer = None;
            mail.reset = true;
        }
        sched::wake(&self.link.controller);
        loop {
            if let Some(answer) = self.link.uas.lock_irq().reset_answer.take() {
                return answer;
            }
            if self.link.is_gone() {
                return Err(PipeError::Gone);
            }
            sched::block_until(sched::now_ns().saturating_add(LOOK_NS), "usb-storage");
        }
    }
}

impl uas::Pipes for UasPipes<'_> {
    fn start(
        &mut self,
        pipe: Pipe,
        n: u16,
        iu: &[u8],
        data: &mut Data<'_>,
    ) -> Result<(), PipeError> {
        match (pipe, data) {
            (Pipe::Command, _) => {
                let Ok(iu) = iu.try_into() else { return Err(PipeError::Failed) };
                self.place_iu(n, iu);
                self.start_command(n);
            }
            (Pipe::Status, _) => self.start_status(n),
            (_, Data::In(buffer)) if buffer.len() <= PAGE_SIZE => {
                let frame = (self.bounce.phys(), buffer.len() as u32);
                self.ask(Action::Start(pipe, n), Some(frame));
            }
            (_, Data::Out(bytes)) if bytes.len() <= PAGE_SIZE => {
                self.bounce.write(0, bytes);
                let frame = (self.bounce.phys(), bytes.len() as u32);
                self.ask(Action::Start(pipe, n), Some(frame));
            }
            (_, Data::Frames { frames, .. }) if frames.len() <= MAX_STREAM_TD_BUFFERS => {
                self.set_data(n, frames);
                self.ask(Action::Start(pipe, n), None);
            }
            _ => return Err(PipeError::Failed),
        }
        Ok(())
    }

    fn cancel(&mut self, pipe: Pipe, n: u16) {
        self.ask(Action::Cancel(pipe, n), None);
    }

    fn wait(
        &mut self,
        ms: u32,
        status: &mut [u8; STATUS_LENGTH],
        data: &mut Data<'_>,
    ) -> Option<(Pipe, u16, Result<usize, PipeError>)> {
        let deadline = sched::now_ns().saturating_add(u64::from(ms) * 1_000_000);
        loop {
            if let Some(ended) = self.take_ended(status) {
                if let ((Pipe::DataIn, _, Ok(came)), Data::In(buffer)) = (ended, data) {
                    let came = came.min(buffer.len());
                    self.bounce.read(0, &mut buffer[..came]);
                }
                return Some(ended);
            }
            let now = sched::now_ns();
            if now >= deadline {
                return None;
            }
            sched::block_until(deadline.min(now.saturating_add(LOOK_NS)), "usb-storage");
        }
    }

    fn reset(&mut self) -> Result<(), PipeError> {
        UasPipes::reset(self)
    }

    fn now_ms(&mut self) -> u64 {
        sched::now_ns() / 1_000_000
    }
}
