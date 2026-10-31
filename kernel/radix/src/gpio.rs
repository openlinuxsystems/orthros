// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! GPIO controllers (Phase 11e, ADR 0233), through `radix-gpio`: AMD's
//! in Zen's FCH and Intel Sunrise Point-LP's, which the ACPI namespace
//! names.
//!
//! [`find`] looks for them once uACPI has started. Each controller's
//! `_CRS` gives its registers and its one interrupt; the kernel turns
//! every pin's interrupt off, then routes the controller's to
//! [`interrupt`], which asks the controller which pins it was for and
//! calls their handlers. A driver whose device signals on a pin, as its
//! `GpioInt` resource says, asks for it with [`request`]: a level-
//! triggered pin is masked when it interrupts, until the driver has
//! served its device and unmasks it ([`Interrupt::unmask`]).
//!
//! The firmware's own events on GPIO pins, which a controller's `_AEI`
//! lists, run its methods as Linux runs them: `_Exx` for an edge on pin
//! `xx` (in hexadecimal) or `_Lxx` for a level, if the controller has
//! one, else `_EVT` with the pin's number; in the thread `gpio-events`,
//! after which a level-triggered pin is unmasked.

use core::ffi::CStr;
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};

use radix_acpi::resources::{self, GpioUsage, Polarity, Resource, Resources, Signal};
use radix_gpio::{Controller, Trigger, Unusable};
use radix_hal::Devices;
use radix_hw::Mmio;
use radix_klog::{TextBuf, error, info, warn};
use radix_sync::{Once, SpinLock};
use radix_uacpi::Node;

use crate::sched::{self, Options, Policy, ThreadRef};
use crate::{Arch, acpi};

/// The controllers' ACPI IDs: AMD's, as Linux's `pinctrl-amd` matches
/// them, and Intel's.
const DEVICES: [(&CStr, Kind); 4] = [
    (c"AMDI0030", Kind::Amd),
    (c"AMD0030", Kind::Amd),
    (c"AMDI0031", Kind::Amd),
    (c"INT344B", Kind::SunrisePointLp),
];

#[derive(Clone, Copy)]
enum Kind {
    Amd,
    SunrisePointLp,
}

/// The most controllers kept, and pins each that interrupt.
const MAX_CONTROLLERS: usize = 4;
const MAX_LINES: usize = 16;
/// The firmware's events kept, over all controllers: a bit each in
/// [`EVENTS`].
const MAX_EVENTS: usize = 64;
/// The most register ranges a controller's `_CRS` gives that are read.
const MAX_RANGES: usize = 4;

/// A controller found.
struct Gpio {
    node: Node,
    /// Its interrupt, if it could be routed.
    gsi: Option<u32>,
    state: SpinLock<State>,
}

struct State {
    controller: Controller,
    lines: [Option<Line>; MAX_LINES],
}

/// A pin that interrupts, and what it calls.
#[derive(Clone, Copy)]
struct Line {
    pin: u16,
    level: bool,
    handler: fn(usize),
    arg: usize,
}

static GPIOS: Once<[Option<Gpio>; MAX_CONTROLLERS]> = Once::new();

/// A firmware's event on a pin: its controller, the pin, and the method
/// it runs.
#[derive(Clone, Copy)]
struct Event {
    interrupt: Interrupt,
    method: Method,
    level: bool,
}

#[derive(Clone, Copy)]
enum Method {
    /// `_Exx` or `_Lxx`, NUL-terminated.
    Own([u8; 5]),
    /// `_EVT`.
    Evt,
}

static EVENT_LIST: Once<[Option<Event>; MAX_EVENTS]> = Once::new();
/// The events that fired, waiting for the thread.
static EVENTS: AtomicU64 = AtomicU64::new(0);
static EVENT_THREAD: Once<ThreadRef> = Once::new();

/// A pin's interrupt, set up by [`request`].
#[derive(Clone, Copy, Debug)]
pub struct Interrupt {
    gpio: usize,
    pin: u16,
}

/// Why a pin's interrupt could not be had.
#[derive(Clone, Copy, Debug)]
pub enum Failed {
    /// The resource is not an interrupt, or names no pin.
    NotInterrupt,
    /// No controller the kernel drives is at the resource's path.
    NoController,
    /// The controller's own interrupt could not be routed.
    NoInterrupt,
    /// The pin interrupts for another already, or the controller has as
    /// many as are kept.
    Taken,
    /// The controller refused the pin.
    Pin(radix_gpio::Error),
}

impl core::fmt::Display for Failed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Failed::NotInterrupt => f.write_str("the resource names no interrupt pin"),
            Failed::NoController => f.write_str("no GPIO controller the kernel drives is there"),
            Failed::NoInterrupt => f.write_str("the GPIO controller's interrupt is not routed"),
            Failed::Taken => f.write_str("the pin is taken, or its controller has no room"),
            Failed::Pin(error) => write!(f, "{error}"),
        }
    }
}

/// Finds the GPIO controllers, sets them up and logs them, then starts
/// the firmware's events on their pins.
pub fn find() {
    if !acpi::running() {
        return;
    }
    let mut found: [Option<(Node, Kind)>; MAX_CONTROLLERS] = [None; MAX_CONTROLLERS];
    for &(hid, kind) in &DEVICES {
        radix_uacpi::find_devices(hid, &mut |node| {
            let known = found.iter().flatten().any(|&(seen, _)| seen == node);
            if let Some(slot) = found.iter_mut().find(|slot| slot.is_none())
                && !known
            {
                *slot = Some((node, kind));
            }
        });
    }
    let mut gpios = [const { None }; MAX_CONTROLLERS];
    let mut interrupts = [None; MAX_CONTROLLERS];
    for (index, &(node, kind)) in found.iter().flatten().enumerate() {
        let Some((controller, interrupt)) = start(node, kind) else { continue };
        gpios[index] = Some(Gpio {
            node,
            gsi: None,
            state: SpinLock::new(State { controller, lines: [None; MAX_LINES] }),
        });
        interrupts[index] = interrupt;
    }
    // Routed once the handler can find each controller.
    for (index, gpio) in gpios.iter_mut().enumerate() {
        let (Some(gpio), Some((gsi, signal))) = (gpio, interrupts[index]) else { continue };
        let level = !signal.edge;
        let low = signal.polarity == Polarity::Low;
        // The vector is kept for good, as the controller is.
        gpio.gsi = Arch::route_interrupt(gsi, level, low, interrupt, index).map(|_| gsi);
    }
    let gpios = GPIOS.call_once(|| gpios);
    for (index, gpio) in gpios.iter().enumerate() {
        let Some(gpio) = gpio else { continue };
        let state = gpio.state.lock_irq();
        let (name, pins) = (state.controller.name(), state.controller.pins());
        drop(state);
        match (gpio.gsi, interrupts[index]) {
            (Some(gsi), Some((_, signal))) => info!(
                "GPIO: {name} controller {}: {pins} pins, interrupt {gsi} ({})",
                gpio.node.path(),
                trigger(signal).unwrap_or(Trigger::LevelLow)
            ),
            (None, Some((gsi, _))) => warn!(
                "GPIO: {name} controller {}: {pins} pins; its interrupt {gsi} could not be \
                 routed, so its pins do not interrupt",
                gpio.node.path()
            ),
            _ => warn!(
                "GPIO: {name} controller {}: {pins} pins; the firmware gives it no interrupt, \
                 so its pins do not interrupt",
                gpio.node.path()
            ),
        }
    }
    start_events(gpios);
}

/// Sets the controller `node` names up: its registers mapped, every pin's
/// interrupt off. Returns it and its interrupt, or logs why not.
fn start(node: Node, kind: Kind) -> Option<(Controller, Option<(u32, Signal)>)> {
    let mut ranges: [Option<(u64, u64)>; MAX_RANGES] = [None; MAX_RANGES];
    let mut interrupt = None;
    let walked = node.bytes(c"_CRS", |bytes| {
        for resource in Resources::new(bytes) {
            match resource {
                Ok(Resource::Memory(memory)) => {
                    if let Some(slot) = ranges.iter_mut().find(|slot| slot.is_none()) {
                        *slot = Some((memory.base, memory.length));
                    }
                }
                Ok(Resource::Interrupts(found)) if interrupt.is_none() => {
                    interrupt = found.numbers().next().map(|gsi| (gsi, found.signal));
                }
                Ok(_) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    });
    match walked {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            warn!("GPIO: {}'s resources cannot be read: {error}; it is not used", node.path());
            return None;
        }
        Err(error) => {
            warn!("GPIO: {} has no resources ({error}); it is not used", node.path());
            return None;
        }
    }
    let mut mapped: [Option<Mmio>; MAX_RANGES] = [const { None }; MAX_RANGES];
    for (slot, &(base, length)) in mapped.iter_mut().zip(ranges.iter().flatten()) {
        let Some(mmio) = usize::try_from(length).ok().and_then(|len| Arch::map_device(base, len))
        else {
            warn!(
                "GPIO: {}'s registers at {base:#x} cannot be mapped; it is not used",
                node.path()
            );
            return None;
        };
        *slot = Some(mmio);
    }
    // AMD's has one range, Sunrise Point-LP's one a community.
    let controller = match (kind, mapped) {
        (Kind::Amd, [Some(registers), ..]) => Controller::amd(registers),
        (Kind::SunrisePointLp, [Some(first), Some(second), Some(third), ..]) => {
            Controller::sunrise_point_lp(&[first, second, third])
        }
        _ => Err(Unusable::Registers),
    };
    match controller {
        Ok(controller) => {
            controller.quiet();
            Some((controller, interrupt))
        }
        Err(why) => {
            warn!("GPIO: {} is not used: {why}", node.path());
            None
        }
    }
}

/// The trigger of a pin that signals as `signal` says.
fn trigger(signal: Signal) -> Option<Trigger> {
    Some(match (signal.edge, signal.polarity) {
        (false, Polarity::High) => Trigger::LevelHigh,
        (false, Polarity::Low) => Trigger::LevelLow,
        (false, Polarity::Both) => return None,
        (true, Polarity::High) => Trigger::RisingEdge,
        (true, Polarity::Low) => Trigger::FallingEdge,
        (true, Polarity::Both) => Trigger::BothEdges,
    })
}

/// A controller's interrupt: calls the handlers of the pins it was for,
/// masking the level-triggered ones first, and any pin no handler has.
fn interrupt(index: usize) {
    let Some(gpio) = GPIOS.get().and_then(|gpios| gpios.get(index)?.as_ref()) else { return };
    let mut fired = [None; MAX_LINES];
    {
        let state = gpio.state.lock_irq();
        let mut count = 0;
        state.controller.pending(&mut |pin| {
            let line = state.lines.iter().flatten().find(|line| line.pin == pin);
            if line.is_none_or(|line| line.level) {
                state.controller.mask(pin);
            }
            if let (Some(&line), Some(slot)) = (line, fired.get_mut(count)) {
                *slot = Some(line);
                count += 1;
            }
        });
        state.controller.finish();
    }
    for line in fired.iter().flatten() {
        (line.handler)(line.arg);
    }
}

/// Has the pin `resource`, a `GpioInt` resource of `device`'s, names
/// call `handler(arg)` from its controller's interrupt, with interrupts
/// masked. A level-triggered pin is masked when it interrupts until
/// [`Interrupt::unmask`]. The pin is unmasked when this returns.
pub fn request(
    device: Node,
    resource: &resources::Gpio<'_>,
    handler: fn(usize),
    arg: usize,
) -> Result<Interrupt, Failed> {
    let GpioUsage::Interrupt(signal) = resource.usage else { return Err(Failed::NotInterrupt) };
    let pin = resource.pins().next().ok_or(Failed::NotInterrupt)?;
    let trigger = trigger(signal).ok_or(Failed::NotInterrupt)?;
    let node = device.find(resource.controller).ok_or(Failed::NoController)?;
    let gpios = GPIOS.get().ok_or(Failed::NoController)?;
    let (index, gpio) = gpios
        .iter()
        .enumerate()
        .find_map(|(index, gpio)| {
            gpio.as_ref().filter(|gpio| gpio.node == node).map(|g| (index, g))
        })
        .ok_or(Failed::NoController)?;
    if gpio.gsi.is_none() {
        return Err(Failed::NoInterrupt);
    }
    let mut state = gpio.state.lock_irq();
    if state.lines.iter().flatten().any(|line| line.pin == pin) {
        return Err(Failed::Taken);
    }
    let slot = state.lines.iter().position(Option::is_none).ok_or(Failed::Taken)?;
    state.controller.set_interrupt(pin, trigger).map_err(Failed::Pin)?;
    state.lines[slot] = Some(Line { pin, level: trigger.is_level(), handler, arg });
    state.controller.unmask(pin);
    Ok(Interrupt { gpio: index, pin })
}

impl Interrupt {
    fn with<T>(self, f: impl FnOnce(&Controller) -> T) -> Option<T> {
        let gpio = GPIOS.get()?.get(self.gpio)?.as_ref()?;
        Some(f(&gpio.state.lock_irq().controller))
    }

    /// Lets the pin interrupt again: a level that is still active
    /// interrupts at once.
    pub fn unmask(self) {
        self.with(|controller| controller.unmask(self.pin));
    }
}

/// Asks for the pins of each controller's `_AEI`, whose events run the
/// firmware's methods, and starts the thread that runs them.
fn start_events(gpios: &[Option<Gpio>; MAX_CONTROLLERS]) {
    let mut events = [None; MAX_EVENTS];
    let mut count = 0;
    for gpio in gpios.iter().flatten() {
        let node = gpio.node;
        let mut list = TextBuf::<96>::new();
        let walked = node.bytes(c"_AEI", |bytes| {
            for resource in Resources::new(bytes) {
                let Ok(Resource::Gpio(resource)) = resource else { continue };
                let Some(pin) = resource.pins().next() else { continue };
                if count == MAX_EVENTS {
                    warn!(
                        "GPIO: more firmware events than the kernel keeps; pin {pin} is not used"
                    );
                    continue;
                }
                let GpioUsage::Interrupt(signal) = resource.usage else { continue };
                let method = own_method(node, pin, signal.edge);
                if method.is_none() && node.find("_EVT").is_none() {
                    warn!("GPIO: {} has no method for its event on pin {pin}", node.path());
                    continue;
                }
                match request(node, &resource, fire, count) {
                    Ok(interrupt) => {
                        let method = method.map_or(Method::Evt, Method::Own);
                        events[count] = Some(Event { interrupt, method, level: !signal.edge });
                        count += 1;
                        let comma = if list.is_empty() { "" } else { ", " };
                        let name = match &method {
                            Method::Own(name) => core::str::from_utf8(&name[..4]).unwrap_or("?"),
                            Method::Evt => "_EVT",
                        };
                        let _ = write!(list, "{comma}{pin} ({name})");
                    }
                    Err(why) => warn!(
                        "GPIO: the firmware's event on {} pin {pin} is not used: {why}",
                        node.path()
                    ),
                }
            }
        });
        if walked.is_ok() && !list.is_empty() {
            info!("GPIO: the firmware's events on {}: pins {list}", node.path());
        }
    }
    if count == 0 {
        return;
    }
    let _ = EVENT_LIST.set(events);
    let options = Options::new().policy(Policy::Fair { nice: -5 });
    match sched::spawn("gpio-events", run_events, 0, options) {
        Ok(thread) => {
            let _ = EVENT_THREAD.set(thread);
        }
        Err(err) => warn!("GPIO: could not start the thread `gpio-events`: {err}"),
    }
}

/// `_Exx` or `_Lxx` for `pin`, if the controller has it.
fn own_method(node: Node, pin: u16, edge: bool) -> Option<[u8; 5]> {
    if pin > 0xff {
        return None;
    }
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let pin = pin as usize;
    let name = [b'_', if edge { b'E' } else { b'L' }, HEX[pin >> 4], HEX[pin & 0xf], 0];
    node.find(core::str::from_utf8(&name[..4]).ok()?).map(|_| name)
}

/// A firmware's event's pin interrupted: the thread runs its method.
fn fire(slot: usize) {
    EVENTS.fetch_or(1 << slot, Ordering::AcqRel);
    if let Some(thread) = EVENT_THREAD.get() {
        sched::wake(thread);
    }
}

/// The thread `gpio-events`: runs the methods of the events that fired.
fn run_events(_: usize) {
    let Some(events) = EVENT_LIST.get() else { return };
    loop {
        let fired = EVENTS.swap(0, Ordering::AcqRel);
        for (slot, event) in events.iter().enumerate() {
            let Some(event) = (*event).filter(|_| fired & (1 << slot) != 0) else { continue };
            let Some(gpio) = GPIOS.get().and_then(|gpios| gpios[event.interrupt.gpio].as_ref())
            else {
                continue;
            };
            let result = match &event.method {
                Method::Own(name) => match CStr::from_bytes_with_nul(name) {
                    Ok(name) => gpio.node.run(name),
                    Err(_) => continue,
                },
                Method::Evt => gpio.node.run_with(c"_EVT", event.interrupt.pin.into()),
            };
            if let Err(error) = result {
                warn!(
                    "GPIO: the firmware's method for {} pin {} failed: {error}",
                    gpio.node.path(),
                    event.interrupt.pin
                );
            }
            if event.level {
                event.interrupt.unmask();
            }
        }
        if EVENTS.load(Ordering::Acquire) == 0 {
            sched::block("gpio");
        }
    }
}

/// `selftest=gpio`, which the `gpio` boot test's SSDT sets up (ADR 0233):
/// QEMU's pc-testdev's memory holds the registers of an AMD and an Intel
/// controller, and their AML methods `FIRE` play the hardware, setting a
/// pin's status and pulsing the controller's interrupt. Each of the
/// firmware's three events must run its method once, the level-triggered
/// pins be unmasked after, and nothing else run.
pub fn selftest() {
    match check() {
        Ok(()) => info!(
            "selftest gpio: passed; the AMD controller's pins 9 (_E09) and 42 (_EVT) and the \
             Intel one's pad 53 (_L35) each ran their method once, and the levels were unmasked \
             after"
        ),
        Err(what) => error!("selftest gpio: failed; {what}"),
    }
}

fn check() -> Result<(), &'static str> {
    let device = |hid: &CStr| {
        let mut found = None;
        radix_uacpi::find_devices(hid, &mut |node| found = Some(node));
        found
    };
    let amd = device(c"AMDI0030").ok_or("the SSDT's AMD controller is not in the namespace")?;
    let intel = device(c"INT344B").ok_or("the SSDT's Intel controller is not in the namespace")?;
    let set_up = |node: Node| {
        GPIOS.get().is_some_and(|gpios| {
            gpios.iter().flatten().any(|gpio| gpio.node == node && gpio.gsi.is_some())
        })
    };
    if !set_up(amd) || !set_up(intel) {
        return Err("a controller was not set up, or its interrupt not routed");
    }
    let fires: [(Node, Option<u64>, u64); 3] =
        [(amd, Some(9), 9), (amd, Some(42), 42), (intel, None, 0x35)];
    for (runs, &(node, pin, wanted)) in (1u64..).zip(&fires) {
        let fired = match pin {
            Some(pin) => node.run_with(c"FIRE", pin),
            None => node.run(c"FIRE"),
        };
        fired.map_err(|_| "a controller's FIRE method failed")?;
        if !wait_for(|| amd.integer(c"\\_SB.RUNS") == Ok(runs)) {
            return Err("an event's method did not run");
        }
        if amd.integer(c"\\_SB.LAST") != Ok(wanted) {
            return Err("an event ran another pin's method");
        }
    }
    // Unmasked once the method has run: pin 42's mask bit set and its
    // status cleared, pad 53's enable bit set.
    let unmasked = || {
        amd.integer(c"RPIN").is_ok_and(|pin| pin & (1 << 12) != 0 && pin & (1 << 28) == 0)
            && intel.integer(c"RENA").is_ok_and(|enabled| enabled & 0x20 != 0)
    };
    if !wait_for(unmasked) {
        return Err("a level-triggered pin was not unmasked after its method ran");
    }
    sched::sleep_ns(50_000_000);
    if amd.integer(c"\\_SB.RUNS") != Ok(3) {
        return Err("an event's method ran more than once");
    }
    Ok(())
}

/// Waits up to a second for `done`.
fn wait_for(done: impl Fn() -> bool) -> bool {
    let deadline = sched::now_ns() + 1_000_000_000;
    while !done() {
        if sched::now_ns() > deadline {
            return false;
        }
        sched::sleep_ns(5_000_000);
    }
    true
}
