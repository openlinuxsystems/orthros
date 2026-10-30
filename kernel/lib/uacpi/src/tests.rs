// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! uACPI on a simulated machine: tables in memory of the test's, I/O ports
//! that keep what is written and clear status bits as ACPI's do, one PCI
//! host bridge, a device register, and a clock that waits take no time on.
//! The DSDT is `tests/dsdt.aml`, made from `tests/dsdt.asl`.
//!
//! uACPI's state is global and starts once, so one test takes it through
//! a whole life: start, AML that uses each of the kernel's services, the
//! power button, a general-purpose event, and powering off.

use std::alloc::Global;
use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::CStr;
use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::*;
use crate::sync::{FOREVER_MS, Semaphore};

const DSDT: &[u8] = include_bytes!("tests/dsdt.aml");

/// Where the tables are in the machine's memory.
const MEMORY: u64 = 0x10_0000;
const MEMORY_SIZE: usize = 0x2000;
const RSDP: u64 = MEMORY;
const XSDT: u64 = MEMORY + 0x40;
const FADT: u64 = MEMORY + 0x100;
const FACS: u64 = MEMORY + 0x240;
const DSDT_AT: u64 = MEMORY + 0x1000;

/// The device register `\_SB.MEMR` reads.
const DEVICE: u64 = 0xfed0_0000;
const DEVICE_VALUE: u32 = 0x1234_5678;

/// The FADT's registers.
const PM1_EVENT: u16 = 0x600;
const PM1_CONTROL: u16 = 0x604;
const PM_TIMER: u16 = 0x608;
const GPE0: u16 = 0x620;
const SCI: u16 = 9;

/// The ports the DSDT's methods write.
const INIT_PORT: u16 = 0x700;
const PTS_PORT: u16 = 0x701;
const GPE_PORT: u16 = 0x702;

struct Machine {
    /// The machine's memory and its device register, as addresses.
    memory: usize,
    device: usize,
    ports: Mutex<HashMap<u16, u8>>,
    /// Every write to a port: port, width, value.
    writes: Mutex<Vec<(u16, u8, u32)>>,
    log: Mutex<Vec<(Level, String)>>,
    start: Instant,
    /// Time stalls and sleeps added, in nanoseconds.
    waited: AtomicU64,
    wakes: (Mutex<u64>, Condvar),
    irq: Mutex<Option<u32>>,
    queued: Mutex<Vec<Queue>>,
}

thread_local! {
    static THREAD: Cell<usize> = const { Cell::new(0) };
}
static THREADS: AtomicUsize = AtomicUsize::new(1);

/// A table header, with room for `length` bytes in all.
fn header(signature: [u8; 4], length: usize, revision: u8) -> Vec<u8> {
    let mut table = vec![0; length];
    table[0..4].copy_from_slice(&signature);
    table[4..8].copy_from_slice(&(length as u32).to_le_bytes());
    table[8] = revision;
    table[10..16].copy_from_slice(b"DUKOVO");
    table[16..24].copy_from_slice(b"TESTTABL");
    table[24..28].copy_from_slice(&1u32.to_le_bytes());
    table
}

/// Makes the bytes from `range` sum to zero through the byte at `at`.
fn checksum(table: &mut [u8], at: usize, range: core::ops::Range<usize>) {
    table[at] = 0;
    let sum = table[range].iter().fold(0u8, |sum, b| sum.wrapping_add(*b));
    table[at] = sum.wrapping_neg();
}

fn put32(table: &mut [u8], at: usize, value: u32) {
    table[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn tables() -> Vec<u8> {
    let mut memory = vec![0; MEMORY_SIZE];
    let mut place = |at: u64, bytes: &[u8]| {
        let at = (at - MEMORY) as usize;
        memory[at..at + bytes.len()].copy_from_slice(bytes);
    };

    let mut rsdp = vec![0; 36];
    rsdp[0..8].copy_from_slice(b"RSD PTR ");
    rsdp[9..15].copy_from_slice(b"DUKOVO");
    rsdp[15] = 2;
    put32(&mut rsdp, 20, 36);
    rsdp[24..32].copy_from_slice(&XSDT.to_le_bytes());
    checksum(&mut rsdp, 8, 0..20);
    checksum(&mut rsdp, 32, 0..36);
    place(RSDP, &rsdp);

    let mut xsdt = header(*b"XSDT", 44, 1);
    xsdt[36..44].copy_from_slice(&FADT.to_le_bytes());
    checksum(&mut xsdt, 9, 0..44);
    place(XSDT, &xsdt);

    let mut fadt = header(*b"FACP", 276, 6);
    put32(&mut fadt, 36, FACS as u32);
    put32(&mut fadt, 40, DSDT_AT as u32);
    fadt[46..48].copy_from_slice(&SCI.to_le_bytes());
    put32(&mut fadt, 56, u32::from(PM1_EVENT));
    put32(&mut fadt, 64, u32::from(PM1_CONTROL));
    put32(&mut fadt, 76, u32::from(PM_TIMER));
    put32(&mut fadt, 80, u32::from(GPE0));
    fadt[88] = 4; // PM1 event block: status and enable, 2 bytes each
    fadt[89] = 2;
    fadt[91] = 4;
    fadt[92] = 2; // GPE0: one status byte, one enable byte
    // WBINVD, and no sleep button as a fixed feature.
    put32(&mut fadt, 112, 1 | 1 << 5);
    fadt[131] = 4;
    checksum(&mut fadt, 9, 0..276);
    place(FADT, &fadt);

    let mut facs = vec![0; 64];
    facs[0..4].copy_from_slice(b"FACS");
    put32(&mut facs, 4, 64);
    facs[32] = 2;
    place(FACS, &facs);

    place(DSDT_AT, DSDT);
    memory
}

impl Machine {
    fn new() -> Machine {
        let memory = Box::leak(tables().into_boxed_slice());
        let device = Box::leak(vec![0u8; 0x1000].into_boxed_slice());
        device[0..4].copy_from_slice(&DEVICE_VALUE.to_le_bytes());
        let mut ports = HashMap::new();
        // Already in ACPI mode: SCI_EN.
        ports.insert(PM1_CONTROL, 1);
        Machine {
            memory: memory.as_mut_ptr().expose_provenance(),
            device: device.as_mut_ptr().expose_provenance(),
            ports: Mutex::new(ports),
            writes: Mutex::new(Vec::new()),
            log: Mutex::new(Vec::new()),
            start: Instant::now(),
            waited: AtomicU64::new(0),
            wakes: (Mutex::new(0), Condvar::new()),
            irq: Mutex::new(None),
            queued: Mutex::new(Vec::new()),
        }
    }

    fn port(&self, port: u16) -> u8 {
        self.ports.lock().unwrap().get(&port).copied().unwrap_or(0)
    }

    fn set_port(&self, port: u16, value: u8) {
        self.ports.lock().unwrap().insert(port, value);
    }

    fn errors(&self) -> Vec<String> {
        let log = self.log.lock().unwrap();
        log.iter().filter(|(level, _)| *level == Level::Error).map(|(_, m)| m.clone()).collect()
    }
}

/// Status registers, whose bits a write of 1 clears.
fn is_status(port: u16) -> bool {
    (PM1_EVENT..PM1_EVENT + 2).contains(&port) || port == GPE0
}

impl Host for Machine {
    fn rsdp(&self) -> Option<u64> {
        Some(RSDP)
    }

    fn map(&self, phys: u64, len: usize) -> Option<usize> {
        let end = phys.checked_add(len as u64)?;
        if phys >= MEMORY && end <= MEMORY + MEMORY_SIZE as u64 {
            Some(self.memory + (phys - MEMORY) as usize)
        } else if phys >= DEVICE && end <= DEVICE + 0x1000 {
            Some(self.device + (phys - DEVICE) as usize)
        } else {
            None
        }
    }

    fn unmap(&self, _: usize, _: usize) {}

    fn heap(&self) -> &(dyn Allocator + Sync) {
        &Global
    }

    fn log(&self, level: Level, message: fmt::Arguments<'_>) {
        let message = message.to_string();
        eprintln!("uACPI {level:?}: {message}");
        self.log.lock().unwrap().push((level, message));
    }

    fn pci_read(&self, address: PciAddress, offset: usize, width: u8) -> Option<u32> {
        if address.segment != 0 || offset + usize::from(width) > 0x1000 {
            return None;
        }
        let at_bridge = address == PciAddress { segment: 0, bus: 0, device: 0, function: 0 };
        let id: u32 = if at_bridge { 0x1237_8086 } else { u32::MAX };
        let shift = (offset % 4) * 8;
        let value = if offset < 4 { id >> shift } else { 0 };
        Some(match width {
            1 => value & 0xff,
            2 => value & 0xffff,
            _ => value,
        })
    }

    fn pci_write(&self, address: PciAddress, offset: usize, width: u8, _: u32) -> bool {
        address.segment == 0 && offset + usize::from(width) <= 0x1000
    }

    fn port_read(&self, port: u16, width: u8) -> u32 {
        if port == PM_TIMER {
            return (self.now_ns() / 279) as u32 & 0xff_ffff;
        }
        (0..u16::from(width)).map(|i| u32::from(self.port(port + i)) << (8 * i)).sum()
    }

    fn port_write(&self, port: u16, width: u8, value: u32) {
        self.writes.lock().unwrap().push((port, width, value));
        for i in 0..u16::from(width) {
            let byte = (value >> (8 * i)) as u8;
            let at = port + i;
            let new = if is_status(at) { self.port(at) & !byte } else { byte };
            self.set_port(at, new);
        }
    }

    fn now_ns(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64 + self.waited.load(Ordering::Relaxed)
    }

    fn stall_us(&self, us: u32) {
        self.waited.fetch_add(u64::from(us) * 1000, Ordering::Relaxed);
    }

    fn sleep_ms(&self, ms: u64) {
        self.waited.fetch_add(ms * 1_000_000, Ordering::Relaxed);
    }

    fn thread_id(&self) -> usize {
        THREAD.with(|id| {
            if id.get() == 0 {
                id.set(THREADS.fetch_add(1, Ordering::Relaxed));
            }
            id.get()
        })
    }

    fn disable_interrupts(&self) -> bool {
        false
    }

    fn restore_interrupts(&self, _: bool) {}

    fn wait(&self, _: usize, deadline_ns: Option<u64>, ready: &dyn Fn() -> bool) -> bool {
        let (lock, wakes) = &self.wakes;
        let mut seen = lock.lock().unwrap();
        loop {
            if ready() {
                return true;
            }
            if deadline_ns.is_some_and(|deadline| self.now_ns() >= deadline) {
                return false;
            }
            seen = wakes.wait_timeout(seen, Duration::from_millis(1)).unwrap().0;
        }
    }

    fn wake_all(&self, _: usize) {
        let (lock, wakes) = &self.wakes;
        *lock.lock().unwrap() += 1;
        wakes.notify_all();
    }

    fn start_interrupt(&self, irq: u32) -> bool {
        *self.irq.lock().unwrap() = Some(irq);
        true
    }

    fn stop_interrupt(&self) {
        *self.irq.lock().unwrap() = None;
    }

    fn work_queued(&self, queue: Queue) {
        self.queued.lock().unwrap().push(queue);
    }
}

static PRESSES: AtomicUsize = AtomicUsize::new(0);

fn pressed() {
    PRESSES.fetch_add(1, Ordering::Relaxed);
}

fn eval(path: &CStr) -> u64 {
    eval_integer(path).unwrap_or_else(|err| panic!("{path:?}: {err}"))
}

#[test]
fn a_machine_runs_its_firmwares_aml_from_start_to_power_off() {
    let machine: &'static Machine = Box::leak(Box::new(Machine::new()));
    start(machine).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(machine.errors(), Vec::<String>::new());
    // uACPI does not list the tables, which the kernel has done, but says
    // what it loaded.
    let said: Vec<String> = machine.log.lock().unwrap().iter().map(|(_, m)| m.clone()).collect();
    assert!(!said.iter().any(|m| m.starts_with("DSDT ") || m.starts_with("RSDP ")), "{said:?}");
    assert!(said.iter().any(|m| m.starts_with("successfully loaded 1 AML blob")), "{said:?}");
    // The namespace's initialization ran the device's `_INI`, and the SCI
    // has a handler.
    assert_eq!(machine.port(INIT_PORT), 0x42);
    assert_eq!(*machine.irq.lock().unwrap(), Some(u32::from(SCI)));

    // A mutex taken and given ten times, and a sleep.
    let before = machine.now_ns();
    assert_eq!(eval(c"\\_SB.SUM"), 55);
    assert!(machine.now_ns() - before >= 1_000_000);
    // A PCI_Config region of the host bridge, and a device register.
    assert_eq!(eval(c"\\_SB.PCI0.HB00.VEND"), 0x8086);
    assert_eq!(eval(c"\\_SB.MEMR"), u64::from(DEVICE_VALUE));
    assert_eq!(
        eval_integer(c"\\_SB.NONE").map_err(Error::code),
        Err(6),
        "a missing object is NOT_FOUND"
    );

    // The power button: its enable bit goes on, and a press reaches the
    // handler and is cleared.
    on_power_button(pressed).unwrap();
    assert_eq!(machine.port(PM1_EVENT + 3) & 1, 1, "PWRBTN_EN");
    machine.set_port(PM1_EVENT + 1, 1);
    assert!(interrupt());
    assert_eq!(PRESSES.load(Ordering::Relaxed), 1);
    assert_eq!(machine.port(PM1_EVENT + 1) & 1, 0, "PWRBTN_STS cleared");

    // GPE 0 has a method, `_L00`: it goes on, and when it fires, it is
    // off until the method has run on the events queue and then, after
    // any `Notify` the method queued, the notify queue has turned it back
    // on, cleared.
    finish_events().unwrap();
    assert_eq!(machine.port(GPE0 + 1) & 1, 1, "GPE 0 enabled");
    machine.set_port(GPE0, 1);
    assert!(interrupt());
    assert_eq!(*machine.queued.lock().unwrap(), [Queue::Events]);
    assert_eq!(machine.port(GPE0 + 1) & 1, 0, "GPE 0 off while its method waits");
    assert!(run_work(Queue::Events));
    assert!(!run_work(Queue::Events));
    assert_eq!(machine.port(GPE_PORT), 1);
    assert_eq!(*machine.queued.lock().unwrap(), [Queue::Events, Queue::Notify]);
    assert!(run_work(Queue::Notify));
    assert!(!run_work(Queue::Notify));
    assert_eq!(machine.port(GPE0) & 1, 0, "GPE 0's status cleared");
    assert_eq!(machine.port(GPE0 + 1) & 1, 1, "GPE 0 enabled again");
    // Nothing raised the interrupt now.
    assert!(!interrupt());

    // Powering off runs `_PTS(5)`, then writes S5's SLP_TYP and SLP_EN; this
    // machine stays on, which uACPI reports after waiting.
    prepare_sleep(SleepState::Off).unwrap();
    assert_eq!(machine.port(PTS_PORT), 5);
    assert_eq!(sleep(SleepState::Off).map_err(Error::code), Err(17), "HARDWARE_TIMEOUT");
    let control = machine.writes.lock().unwrap();
    let control: Vec<u32> =
        control.iter().filter(|(port, ..)| *port == PM1_CONTROL).map(|w| w.2).collect();
    assert!(control.contains(&(1 | 5 << 10 | 1 << 13)), "{control:x?}");
    drop(control);
    assert_eq!(machine.errors(), Vec::<String>::new());
}

#[test]
fn a_semaphore_hands_its_units_out_and_waits_for_more() {
    let machine: &'static Machine = Box::leak(Box::new(Machine::new()));
    let semaphore: &'static Semaphore = Box::leak(Box::new(Semaphore::new(1)));
    assert!(semaphore.take(machine, 0));
    assert!(!semaphore.take(machine, 0), "none left");
    // A timed wait runs out on the clock.
    let before = machine.now_ns();
    assert!(!semaphore.take(machine, 5));
    assert!(machine.now_ns() - before >= 5_000_000);
    // A waiter for good wakes when another thread gives a unit.
    let waiter = thread::spawn(move || semaphore.take(machine, FOREVER_MS));
    thread::sleep(Duration::from_millis(20));
    semaphore.give(machine);
    assert!(waiter.join().unwrap());
    // Units given with no one waiting are kept.
    semaphore.give(machine);
    semaphore.give(machine);
    assert!(semaphore.take(machine, 0));
    assert!(semaphore.take(machine, 0));
    assert!(!semaphore.take(machine, 0));
}

#[test]
fn handles_name_what_they_stand_for() {
    use crate::ffi::{io_handle, io_port, pci_address, pci_handle};
    let address = PciAddress { segment: 0xffff, bus: 0xff, device: 31, function: 7 };
    assert_eq!(pci_address(pci_handle(address)), address);
    let address = PciAddress { segment: 0, bus: 0, device: 0, function: 0 };
    assert!(!pci_handle(address).is_null());
    assert_eq!(pci_address(pci_handle(address)), address);

    let range = io_handle(0xb2, 2);
    assert!(!range.is_null());
    assert_eq!(io_port(range, 0, 1), Some(0xb2));
    assert_eq!(io_port(range, 1, 1), Some(0xb3));
    assert_eq!(io_port(range, 0, 2), Some(0xb2));
    assert_eq!(io_port(range, 1, 2), None, "past the range");
    assert_eq!(io_port(range, usize::MAX, 1), None);
    let last = io_handle(0xfffc, 4);
    assert_eq!(io_port(last, 0, 4), Some(0xfffc));
    assert_eq!(io_port(last, 3, 1), Some(0xffff));
    assert_eq!(io_port(last, 4, 1), None);
}
