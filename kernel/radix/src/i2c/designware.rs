// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The DesignWare I2C controllers (Phase 11e, ADR 0234), through
//! `radix-i2c`: Intel Sunrise Point's in its LPSS, PCI functions
//! 8086:9d60 to 9d65 (LP) and a160 to a163 (H) or, in the firmware's ACPI
//! mode, ACPI devices `INT3442` to `INT3447`; and AMD's FCH's, ACPI
//! devices `AMDI0010` and `AMD0010`.
//!
//! The PCI scan keeps the LPSS functions ([`bind_lpss`]); once uACPI
//! runs, [`find`] finds each function's ACPI node by its `_ADR`, and
//! AMD's controllers and LPSS's in ACPI mode by their `_HID`. The devices
//! on a bus are those present whose `_CRS` has an `I2cSerialBus` resource
//! naming its controller, as Linux finds them; the bus runs at the
//! fastest speed all of them allow, fast mode from 400 kHz, or fast mode
//! if none is named. Each controller is then powered on (its `_PS0`, and
//! a PCI function's D0), an LPSS function taken out of reset
//! ([`radix_i2c::lpss`]), and set up with the firmware's SCL counts for
//! the speed (`SSCN` or `FMCN`) if it has them, else counts for the
//! controller's clock: 120 MHz on Intel's, with SDA held 230 ns, 150 MHz
//! on `AMDI0010`'s and 133 MHz on `AMD0010`'s, as Linux takes them.
//!
//! The buses are `i2c-1` onwards (the SMBus is `i2c-0`), in the order
//! found, and a transfer ([`transfer`]) holds its bus's mutex, polling
//! the controller asleep.

use core::ffi::CStr;
use core::sync::atomic::{AtomicUsize, Ordering};

use radix_acpi::resources::{Resource, Resources};
use radix_hal::{Devices, Timer};
use radix_hw::pci::{self, Bar, Function};
use radix_i2c::designware::{self, DesignWare, Timing};
use radix_i2c::{Error, Message, Speed, lpss};
use radix_klog::{TextBuf, error, info, warn};
use radix_sync::{Mutex, Once, SpinLock};
use radix_uacpi::power::Fields;
use radix_uacpi::{Hid, Node};

use crate::{Arch, acpi, sched};

/// Sunrise Point's LPSS I2C functions: LP's six, then H's four.
const LPSS_DEVICES: [u16; 10] =
    [0x9d60, 0x9d61, 0x9d62, 0x9d63, 0x9d64, 0x9d65, 0xa160, 0xa161, 0xa162, 0xa163];

/// The controllers' ACPI IDs: LPSS's in ACPI mode, then AMD's.
const ACPI_DEVICES: [(&CStr, Kind); 8] = [
    (c"INT3442", Kind::Lpss),
    (c"INT3443", Kind::Lpss),
    (c"INT3444", Kind::Lpss),
    (c"INT3445", Kind::Lpss),
    (c"INT3446", Kind::Lpss),
    (c"INT3447", Kind::Lpss),
    (c"AMDI0010", Kind::Amd(150_000_000)),
    (c"AMD0010", Kind::Amd(133_000_000)),
];

#[derive(Clone, Copy)]
enum Kind {
    Lpss,
    /// AMD's, with its clock in hertz.
    Amd(u64),
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Lpss => "Intel Sunrise Point's",
            Kind::Amd(_) => "AMD's",
        }
    }

    fn clock_hz(self) -> u64 {
        match self {
            Kind::Lpss => lpss::CLOCK_HZ,
            Kind::Amd(hz) => hz,
        }
    }

    /// SDA's hold time where the firmware gives none.
    fn hold_ns(self) -> Option<u64> {
        match self {
            Kind::Lpss => Some(lpss::SDA_HOLD_NS),
            Kind::Amd(_) => None,
        }
    }
}

/// The most buses kept, and devices over all of them.
const MAX_BUSES: usize = 8;
const MAX_DEVICES: usize = 16;
/// The first bus's number: `i2c-0` is the SMBus.
const FIRST_NUMBER: usize = 1;

/// The LPSS functions the PCI scan found.
static FUNCTIONS: SpinLock<[Option<Function>; MAX_BUSES]> =
    SpinLock::new([const { None }; MAX_BUSES]);

/// A bus found: its controller, set up.
struct Bus {
    node: Option<Node>,
    kind: Kind,
    pci: Option<pci::Address>,
    speed: Speed,
    controller: Mutex<DesignWare<Waits>>,
}

static BUSES: [Once<Bus>; MAX_BUSES] = [const { Once::new() }; MAX_BUSES];
static COUNT: AtomicUsize = AtomicUsize::new(0);

/// A device on a bus, as the firmware names it.
#[derive(Clone, Copy)]
pub struct Device {
    /// Its bus's number, as in `i2c-1`.
    pub bus: usize,
    /// Its node.
    pub node: Node,
    /// Its address.
    pub address: u16,
    /// Whether the address is a 10-bit one.
    pub ten_bit: bool,
    /// The fastest it allows, in hertz.
    pub speed: u32,
    /// Its `_HID`.
    pub hid: Option<Hid>,
    /// Its place among the devices of its `_HID`, as in Linux's name for
    /// it, `PNP0C50:00`.
    pub instance: u8,
}

/// The devices, in the order the namespace has them; while [`find`]
/// runs, `bus` is the place of its controller among those found.
static DEVICES: SpinLock<([Option<Device>; MAX_DEVICES], usize)> =
    SpinLock::new(([None; MAX_DEVICES], 0));

/// A controller found, before it is set up.
#[derive(Clone, Copy)]
struct Found {
    node: Option<Node>,
    kind: Kind,
    /// Its place in [`FUNCTIONS`], for an LPSS function.
    function: Option<usize>,
}

/// The waits of a transfer: asleep where the caller may sleep, and spins.
struct Waits;

impl radix_i2c::Platform for Waits {
    fn wait_us(&self, us: u64) {
        super::wait(us);
    }

    fn spin_us(&self, us: u64) {
        Arch::delay_us(us);
    }
}

/// Whether `function` is one of LPSS's I2C controllers.
pub fn is_lpss(function: &Function) -> bool {
    function.vendor_id() == 0x8086 && LPSS_DEVICES.contains(&function.device_id())
}

/// Keeps the LPSS function for [`find`], from the PCI scan.
pub fn bind_lpss(function: &Function) {
    let mut functions = FUNCTIONS.lock();
    match functions.iter_mut().find(|slot| slot.is_none()) {
        Some(slot) => *slot = Some(function.clone()),
        None => {
            drop(functions);
            warn!("I2C: too many controllers; the one at {} is left alone", function.address());
        }
    }
}

/// Finds the controllers and the devices on their buses, sets each up and
/// logs it.
pub fn find() {
    if !acpi::running() {
        if FUNCTIONS.lock().iter().any(Option::is_some) {
            info!("I2C: without the firmware's AML, the LPSS controllers are left alone");
        }
        return;
    }
    let mut found = [None; MAX_BUSES];
    let mut count = 0;
    for index in 0..MAX_BUSES {
        let Some(address) = FUNCTIONS.lock()[index].as_ref().map(Function::address) else {
            continue;
        };
        found[count] =
            Some(Found { node: companion(address), kind: Kind::Lpss, function: Some(index) });
        count += 1;
    }
    for &(hid, kind) in &ACPI_DEVICES {
        radix_uacpi::find_devices(hid, &mut |node| {
            let known = found.iter().flatten().any(|seen: &Found| seen.node == Some(node));
            if count < MAX_BUSES && !known {
                found[count] = Some(Found { node: Some(node), kind, function: None });
                count += 1;
            }
        });
    }
    if count == 0 {
        return;
    }
    find_devices(&found);
    let mut numbers = [None; MAX_BUSES];
    for (place, found) in found.iter().enumerate() {
        let Some(found) = found else { continue };
        let speed = bus_speed(place);
        let Some((controller, firmware)) = start(found, speed) else { continue };
        let index = COUNT.load(Ordering::Relaxed);
        let depths = controller.depths();
        let pci = found.function.and_then(|i| FUNCTIONS.lock()[i].as_ref().map(Function::address));
        BUSES[index].call_once(|| Bus {
            node: found.node,
            kind: found.kind,
            pci,
            speed,
            controller: Mutex::new(controller),
        });
        COUNT.store(index + 1, Ordering::Release);
        numbers[place] = Some(FIRST_NUMBER + index);
        log_bus(index, depths.0, firmware);
    }
    // The devices of controllers not set up are dropped, and the others
    // take their bus's number.
    let mut devices = DEVICES.lock();
    let (list, kept) = &mut *devices;
    let mut write = 0;
    for read in 0..*kept {
        let Some(mut device) = list[read] else { continue };
        if let Some(number) = numbers[device.bus] {
            device.bus = number;
            list[write] = Some(device);
            write += 1;
        }
    }
    list[write..].fill(None);
    *kept = write;
    drop(devices);
    for_each_device(&mut |device| {
        let hid = device.hid;
        info!(
            "I2C: i2c-{} has {} ({}) at {:#04x}{}, up to {} kHz",
            device.bus,
            device.node.path(),
            hid.as_ref().map_or("no _HID", Hid::as_str),
            device.address,
            if device.ten_bit { " (10-bit)" } else { "" },
            device.speed / 1000
        );
    });
}

/// Logs bus `index`, set up.
fn log_bus(index: usize, fifo: usize, firmware: bool) {
    let Some(bus) = BUSES[index].get() else { return };
    let mut place = TextBuf::<96>::new();
    use core::fmt::Write;
    match (bus.node, bus.pci) {
        (Some(node), Some(pci)) => {
            let _ = write!(place, "{} ({pci})", node.path());
        }
        (Some(node), None) => {
            let _ = write!(place, "{}", node.path());
        }
        (None, Some(pci)) => {
            let _ = write!(place, "{pci}, which the firmware has no node for,");
        }
        (None, None) => {}
    }
    let mut counts = TextBuf::<48>::new();
    if firmware {
        let _ = counts.write_str("the firmware's counts");
    } else {
        let _ = write!(counts, "counts for its {} MHz clock", bus.kind.clock_hz() / 1_000_000);
    }
    let (mode, khz) = match bus.speed {
        Speed::Standard => ("standard", 100),
        Speed::Fast => ("fast", 400),
    };
    info!(
        "I2C: {} controller {place} is i2c-{}: {mode} mode ({khz} kHz), {fifo}-byte FIFOs, {counts}",
        bus.kind.name(),
        FIRST_NUMBER + index
    );
}

/// The ACPI node of the PCI function at `address`: the device whose
/// `_ADR` is its device and function, in a PCI root bridge whose `_BBN`
/// is its bus.
fn companion(address: pci::Address) -> Option<Node> {
    let wanted = (u64::from(address.device) << 16) | u64::from(address.function);
    let mut found = None;
    radix_uacpi::find_all_devices(&mut |node| {
        if found.is_some() || node.address() != Ok(wanted) {
            return;
        }
        let Some(parent) = node.parent() else { return };
        let root = parent.hid().is_some_and(|hid| matches!(hid.as_str(), "PNP0A08" | "PNP0A03"));
        let bus = match parent.integer(c"_BBN") {
            Ok(bus) => bus,
            Err(error) if error.is_not_found() => 0,
            Err(_) => return,
        };
        if root && bus == u64::from(address.bus) {
            found = Some(node);
        }
    });
    found
}

/// Finds the devices present whose `_CRS` names one of the controllers
/// `found`, and keeps them in [`DEVICES`].
fn find_devices(found: &[Option<Found>; MAX_BUSES]) {
    radix_uacpi::find_all_devices(&mut |node| {
        if node.find("_CRS").is_none() || !node.present() {
            return;
        }
        let _ = node.bytes(c"_CRS", |bytes| {
            for resource in Resources::new(bytes) {
                let Ok(Resource::I2c(i2c)) = resource else { continue };
                let Some(controller) = node.find(i2c.controller) else { continue };
                let Some(place) =
                    found.iter().position(|f| f.is_some_and(|f| f.node == Some(controller)))
                else {
                    continue;
                };
                keep(Device {
                    bus: place,
                    node,
                    address: i2c.address,
                    ten_bit: i2c.ten_bit,
                    speed: i2c.speed,
                    hid: node.hid(),
                    instance: 0,
                });
            }
        });
    });
}

/// Keeps `device`, numbered among those of its `_HID`.
fn keep(mut device: Device) {
    let mut devices = DEVICES.lock();
    let (list, count) = &mut *devices;
    let same = list.iter().flatten().filter(|d| d.hid.is_some() && d.hid == device.hid).count();
    device.instance = u8::try_from(same).unwrap_or(u8::MAX);
    if let Some(slot) = list.get_mut(*count) {
        *slot = Some(device);
        *count += 1;
    }
}

/// The speed of the bus of the controller at `place`: the fastest all its
/// devices allow, or fast mode.
fn bus_speed(place: usize) -> Speed {
    let mut slowest = None;
    for_each_device(&mut |device| {
        if device.bus == place {
            slowest = Some(slowest.map_or(device.speed, |s: u32| s.min(device.speed)));
        }
    });
    slowest.map_or(Speed::Fast, Speed::for_devices)
}

/// Calls `f` with each device kept, one at a time, without the lock held.
fn for_each_device(f: &mut dyn FnMut(&Device)) {
    for index in 0..MAX_DEVICES {
        let Some(device) = DEVICES.lock().0[index] else { break };
        f(&device);
    }
}

/// Calls `f` with each device on a bus, as the firmware names it.
pub fn devices(f: &mut dyn FnMut(&Device)) {
    if COUNT.load(Ordering::Acquire) > 0 {
        for_each_device(f);
    }
}

/// Calls `f` with each bus's number.
pub fn buses(f: &mut dyn FnMut(usize)) {
    for index in 0..COUNT.load(Ordering::Acquire) {
        f(FIRST_NUMBER + index);
    }
}

/// Runs `messages` with the device at `address` on bus `number`, as
/// [`DesignWare::transfer`]: [`Error::Invalid`] if there is no such bus.
/// From a thread that may sleep.
pub fn transfer(
    number: usize,
    address: u16,
    ten_bit: bool,
    messages: &mut [Message<'_>],
) -> Result<(), Error> {
    let index = number.checked_sub(FIRST_NUMBER).ok_or(Error::Invalid)?;
    if index >= COUNT.load(Ordering::Acquire) {
        return Err(Error::Invalid);
    }
    let bus = BUSES[index].get().ok_or(Error::Invalid)?;
    bus.controller.lock().transfer(address, ten_bit, messages)
}

/// Powers the controller `found` on, maps it and sets it up for `speed`;
/// returns it and whether the firmware gave its counts, or logs why not.
fn start(found: &Found, speed: Speed) -> Option<(DesignWare<Waits>, bool)> {
    let what = |text: &mut TextBuf<96>| {
        use core::fmt::Write;
        match (found.node, found.function) {
            (Some(node), _) => write!(text, "{}", node.path()),
            (None, Some(index)) => match FUNCTIONS.lock()[index].as_ref() {
                Some(function) => write!(text, "{}", function.address()),
                None => Ok(()),
            },
            (None, None) => Ok(()),
        }
    };
    let mut name = TextBuf::<96>::new();
    let _ = what(&mut name);
    if let Some(node) = found.node
        && node.find("_PS0").is_some()
        && let Err(error) = node.run(c"_PS0")
    {
        warn!("I2C: {name}'s _PS0 failed: {error}");
    }
    let (window, address) = match found.function {
        Some(index) => pci_window(index)?,
        None => acpi_window(found.node?)?,
    };
    let Some(registers) =
        usize::try_from(window.1).ok().and_then(|len| Arch::map_device(window.0, len.min(0x1000)))
    else {
        warn!("I2C: {name}'s registers at {:#x} cannot be mapped; it is not used", window.0);
        return None;
    };
    if let Kind::Lpss = found.kind {
        let Some(private) = registers.window(lpss::PRIVATE, lpss::PRIVATE_SIZE) else {
            warn!("I2C: {name}'s registers are too few for LPSS's; it is not used");
            return None;
        };
        if let Err(not) = lpss::set_up(&private, address) {
            info!("I2C: {name} is not an I2C controller (LPSS type {}); it is left alone", not.0);
            return None;
        }
    }
    let firmware = found.node.and_then(|node| firmware_timing(node, speed, found.kind));
    let timing = firmware
        .unwrap_or_else(|| Timing::computed(speed, found.kind.clock_hz(), found.kind.hold_ns()));
    let Some(controller) = registers.window(0, designware::SIZE) else {
        warn!("I2C: {name}'s registers are too few; it is not used");
        return None;
    };
    match DesignWare::new(controller, Waits, speed, timing) {
        Ok(controller) => Some((controller, firmware.is_some())),
        Err(why) => {
            warn!("I2C: {name} is not used: {why}");
            None
        }
    }
}

/// The LPSS function's registers, (address, size), and the address,
/// with the function powered on and answering.
fn pci_window(index: usize) -> Option<((u64, u64), u64)> {
    let function = FUNCTIONS.lock()[index].clone()?;
    let Some(Bar::Memory { address, size, .. }) = function.bar(0) else {
        warn!("I2C: the LPSS function at {} has no registers", function.address());
        return None;
    };
    // D0, from D3hot: then 10 ms before it answers (PCI PM 1.2).
    if let Some(pm) = function.find_capability(PM_CAPABILITY) {
        let control = function.read16(pm + 4);
        if control & 0x3 != 0 {
            function.write16(pm + 4, control & !0x3);
            sched::sleep_ns(10_000_000);
        }
    }
    function.set_command(function.command() | pci::command::MEMORY);
    Some(((address, size), address))
}

/// The PCI power management capability's ID.
const PM_CAPABILITY: u8 = 0x01;

/// The registers an ACPI device's `_CRS` gives, (address, size), and the
/// address.
fn acpi_window(node: Node) -> Option<((u64, u64), u64)> {
    let memory = node.bytes(c"_CRS", |bytes| {
        Resources::new(bytes).find_map(|resource| match resource {
            Ok(Resource::Memory(memory)) => Some((memory.base, memory.length)),
            _ => None,
        })
    });
    match memory {
        Ok(Some(range)) => Some((range, range.0)),
        _ => {
            warn!("I2C: {} has no registers in its resources; it is not used", node.path());
            None
        }
    }
}

/// The firmware's SCL counts and SDA hold time for `speed`: its `SSCN` or
/// `FMCN` package, with the kind's hold time if its own is 0.
fn firmware_timing(node: Node, speed: Speed, kind: Kind) -> Option<Timing> {
    let method = match speed {
        Speed::Standard => c"SSCN",
        Speed::Fast => c"FMCN",
    };
    let name = method.to_str().unwrap_or("");
    node.find(name)?;
    let counts = node.package(method, |package| {
        let count = |index| package.integer(index).and_then(|v| u16::try_from(v).ok());
        Some((count(0)?, count(1)?, count(2).unwrap_or(0)))
    });
    let Ok(Some((high, low, hold))) = counts else {
        warn!("I2C: {}'s {name} is not three counts; counts are computed", node.path());
        return None;
    };
    let hold = match hold {
        0 => Timing::computed(speed, kind.clock_hz(), kind.hold_ns()).hold,
        hold => Some(hold),
    };
    (high > 0 && low > 0).then_some(Timing { high, low, hold })
}

/// `selftest=i2c`: on the `i2c` boot test's SSDT, checks what the kernel
/// set its two controllers up with, and a transfer on each.
pub fn selftest() {
    match check() {
        Ok(()) => info!(
            "selftest i2c: passed; the Intel controller in fast mode with the firmware's \
             counts, out of LPSS's reset, the AMD one in standard mode with computed counts, \
             and a transfer on each"
        ),
        Err(what) => error!("selftest i2c: failed; {what}"),
    }
}

/// The registers an SSDT controller's `REGS` method reads back: fields
/// read into its package, as the namespace's fields are not values.
fn registers(node: Node) -> Result<[u64; 10], &'static str> {
    node.package(c"REGS", |package| {
        core::array::from_fn(|index| package.integer(index).unwrap_or(u64::MAX))
    })
    .map_err(|_| "an SSDT controller's REGS method failed")
}

fn check() -> Result<(), &'static str> {
    let device = |hid: &CStr| {
        let mut found = None;
        radix_uacpi::find_devices(hid, &mut |node| found = Some(node));
        found
    };
    let intel = device(c"INT3443").ok_or("the SSDT's Intel controller is not in the namespace")?;
    let amd = device(c"AMDI0010").ok_or("the SSDT's AMD controller is not in the namespace")?;
    if intel.integer(c"PWR0") != Ok(1) {
        return Err("the Intel controller's _PS0 did not run");
    }
    let mut seen = 0;
    let mut wrong = false;
    devices(&mut |d| {
        let hid = d.hid.as_ref().map_or("", Hid::as_str);
        let expected = match seen {
            0 => (1, 0x2c, 400_000, "DUKV0001"),
            _ => (2, 0x48, 100_000, "DUKV0002"),
        };
        wrong |= (d.bus, d.address, d.speed, hid) != expected || d.ten_bit;
        seen += 1;
    });
    if wrong || seen != 2 {
        return Err("the devices on the buses are not the SSDT's two");
    }
    // The Intel controller's memory says a stop was seen, so a transfer
    // that only writes ends once queued; the AMD one's that no device
    // acknowledged its address.
    transfer(1, 0x2c, false, &mut [Message::Write(&[0x05, 0x01])])
        .map_err(|_| "a write on i2c-1 failed")?;
    let mut byte = [0];
    if transfer(2, 0x48, false, &mut [Message::Read(&mut byte)]) != Err(Error::NoDevice) {
        return Err("a refused read on i2c-2 was not reported as no device");
    }
    // Then the Intel controller: master, fast mode, repeated starts, no
    // slave; the device's address, and the last byte with a stop; FMCN's
    // counts; its hold, with a cycle's receive hold; polled; out of
    // LPSS's reset, and its DMA engine told the window's address. The AMD
    // one: standard mode; 150 MHz's counts; the hold left as it was.
    let intel_wanted = [0x65, 0x2c, 0x201, 0x88, 0xa8, 0x1_001e, 0, 7, 0xff00_0000, 0];
    if registers(intel)? != intel_wanted {
        return Err("the Intel controller was not set up as the SSDT asks");
    }
    if registers(amd)?[..4] != [0x63, 642, 749, 5] {
        return Err("the AMD controller was not set up for standard mode at 150 MHz");
    }
    if transfer(3, 0x10, false, &mut [Message::Read(&mut byte)]) != Err(Error::Invalid) {
        return Err("a transfer on a bus that does not exist was not refused");
    }
    Ok(())
}
