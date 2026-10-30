// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The border with the C: the parts of uACPI's API the kernel calls, and
//! the `uacpi_kernel_*` functions uACPI calls, each on top of [`Host`].
//!
//! uACPI's handles are pointers the kernel makes up. Here a semaphore's
//! and a spin lock's point to them on the heap; a PCI function's and an
//! I/O range's are their numbers, never read through; a work item's is
//! a token, as the queues keep room for each item instead
//! (`crate::work`). Every pointer uACPI passes is trusted to be what its
//! header (`vendor/include/uacpi/kernel_api.h`) says.

#![allow(unsafe_code)]

use alloc::boxed::Box;
use core::alloc::{Allocator, Layout};
use core::ffi::{CStr, c_char, c_ulong, c_void};
use core::fmt;
use core::ptr::{self, NonNull};

use radix_sync::{Once, SpinLock};

use crate::sync::{Semaphore, Spin};
use crate::work::{self, Handler, Work};
use crate::{HOST, Host, Level, PciAddress, Queue, SleepState, host};

type Status = u32;
type Handle = *mut c_void;
type InterruptHandler = unsafe extern "C" fn(Handle) -> u32;

const OK: Status = 0;
const NOT_FOUND: Status = 6;
const INVALID_ARGUMENT: Status = 7;
const ALREADY_EXISTS: Status = 9;
const INTERNAL_ERROR: Status = 10;
const TIMEOUT: Status = 18;

/// `uacpi_interrupt_ret`: the interrupt was the handler's.
const INTERRUPT_HANDLED: u32 = 1;
/// `uacpi_fixed_event`: the power button.
const FIXED_EVENT_POWER_BUTTON: u32 = 2;
/// `uacpi_sleep_state`: S5.
const SLEEP_STATE_S5: u32 = 5;
/// `uacpi_work_type`: a general-purpose event's method.
const WORK_GPE_EXECUTION: u32 = 0;
/// `uacpi_firmware_request_type`: AML's `Fatal`.
const FIRMWARE_REQUEST_FATAL: u8 = 1;
/// `uacpi_log_level`: errors and warnings only.
const LOG_WARN: u32 = 2;
/// `uacpi_log_level`: informational messages too, the default.
const LOG_INFO: u32 = 3;

/// What `uacpi_kernel_map` returns when it cannot map.
const MAP_FAILED: Handle = ptr::without_provenance_mut(usize::MAX);

/// Alignment of uACPI's memory: what `malloc` gives.
const ALIGN: usize = 16;

/// `uacpi_pci_address`.
#[repr(C)]
#[derive(Clone, Copy)]
struct RawPciAddress {
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
}

/// `uacpi_firmware_request`.
#[repr(C)]
struct FirmwareRequest {
    kind: u8,
    body: RequestBody,
}

#[repr(C)]
union RequestBody {
    breakpoint: Handle,
    fatal: Fatal,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Fatal {
    kind: u8,
    code: u32,
    arg: u64,
}

unsafe extern "C" {
    fn uacpi_context_set_log_level(level: u32);
    fn uacpi_initialize(flags: u64) -> Status;
    fn uacpi_namespace_load() -> Status;
    fn uacpi_namespace_initialize() -> Status;
    fn uacpi_finalize_gpe_initialization() -> Status;
    fn uacpi_install_fixed_event_handler(
        event: u32,
        handler: InterruptHandler,
        user: Handle,
    ) -> Status;
    fn uacpi_prepare_for_sleep_state(state: u32) -> Status;
    fn uacpi_enter_sleep_state(state: u32) -> Status;
    fn uacpi_eval_simple_integer(parent: Handle, path: *const c_char, out: *mut u64) -> Status;
    fn uacpi_status_to_string(status: Status) -> *const c_char;
}

/// One of uACPI's errors: a `uacpi_status` other than success.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error(Status);

impl Error {
    /// The `uacpi_status` number.
    pub fn code(self) -> u32 {
        self.0
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: uACPI returns a static NUL-terminated string for every
        // status, "<invalid status>" for one it does not know.
        let text = unsafe { CStr::from_ptr(uacpi_status_to_string(self.0)) };
        write!(f, "{}", text.to_str().unwrap_or("?"))
    }
}

fn check(status: Status) -> Result<(), Error> {
    if status == OK { Ok(()) } else { Err(Error(status)) }
}

/// What [`start`] was doing when uACPI failed, and how.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StartError {
    /// "read the tables", "load the namespace" or "initialize the
    /// namespace".
    pub step: &'static str,
    /// How uACPI failed.
    pub error: Error,
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "could not {}: {}", self.step, self.error)
    }
}

/// Starts uACPI on `host`: reads the tables, takes the machine into ACPI
/// mode, loads the DSDT's and SSDTs' AML into the namespace, starts the
/// System Control Interrupt and runs the devices' `_STA` and `_INI`
/// methods. General-purpose events stay off until [`finish_events`].
/// Called once, from a thread that may sleep.
///
/// The kernel lists the tables itself, so uACPI says only what goes
/// wrong while it reads them; then what it loads and finds.
pub fn start(host: &'static dyn Host) -> Result<(), StartError> {
    let fail = |step| move |error| StartError { step, error };
    if HOST.set(host).is_err() {
        return Err(StartError { step: "start", error: Error(ALREADY_EXISTS) });
    }
    // SAFETY: uACPI's entry points, called once each and in order, by one
    // thread; its callbacks below have the host they need.
    unsafe {
        uacpi_context_set_log_level(LOG_WARN);
        check(uacpi_initialize(0)).map_err(fail("read the tables"))?;
        uacpi_context_set_log_level(LOG_INFO);
        check(uacpi_namespace_load()).map_err(fail("load the namespace"))?;
        check(uacpi_namespace_initialize()).map_err(fail("initialize the namespace"))?;
    }
    Ok(())
}

/// Turns on each general-purpose event that has an AML method, once the
/// kernel's own handlers are in. Called once, after [`start`].
pub fn finish_events() -> Result<(), Error> {
    // SAFETY: called after `start`, from a thread that may sleep.
    check(unsafe { uacpi_finalize_gpe_initialization() })
}

static POWER_BUTTON: Once<fn()> = Once::new();

/// Calls `pressed`, in an interrupt handler with interrupts masked, each
/// time the power button that is ACPI's fixed feature is pressed. Called
/// once, after [`start`].
pub fn on_power_button(pressed: fn()) -> Result<(), Error> {
    if POWER_BUTTON.set(pressed).is_err() {
        return Err(Error(ALREADY_EXISTS));
    }
    // SAFETY: `power_button` takes no context and lives for good.
    check(unsafe {
        uacpi_install_fixed_event_handler(FIXED_EVENT_POWER_BUTTON, power_button, ptr::null_mut())
    })
}

extern "C" fn power_button(_: Handle) -> u32 {
    if let Some(pressed) = POWER_BUTTON.get() {
        pressed();
    }
    INTERRUPT_HANDLED
}

fn sleep_state(state: SleepState) -> u32 {
    match state {
        SleepState::Off => SLEEP_STATE_S5,
    }
}

/// Runs the AML that readies the machine for `state` (`_PTS`, `_GTS`).
/// From a thread that may sleep.
pub fn prepare_sleep(state: SleepState) -> Result<(), Error> {
    // SAFETY: called after `start`, from a thread that may sleep.
    check(unsafe { uacpi_prepare_for_sleep_state(sleep_state(state)) })
}

/// Enters `state`, after [`prepare_sleep`], with interrupts masked. For
/// [`SleepState::Off`], returns only if the machine stayed on.
pub fn sleep(state: SleepState) -> Result<(), Error> {
    // SAFETY: called after `prepare_sleep`, as uACPI requires.
    check(unsafe { uacpi_enter_sleep_state(sleep_state(state)) })
}

/// Evaluates the object at `path` (absolute, as `\_SB.PCI0._ADR`) as an
/// integer. From a thread that may sleep.
pub fn eval_integer(path: &CStr) -> Result<u64, Error> {
    let mut value = 0;
    // SAFETY: a null parent makes the path absolute; `path` is
    // NUL-terminated and `value` is written once uACPI returns success.
    check(unsafe { uacpi_eval_simple_integer(ptr::null_mut(), path.as_ptr(), &raw mut value) })?;
    Ok(value)
}

/// The System Control Interrupt's handler, which uACPI installs.
#[derive(Clone, Copy)]
struct Interrupt {
    handler: InterruptHandler,
    ctx: usize,
}

static INTERRUPT: SpinLock<Option<Interrupt>> = SpinLock::new(None);

/// The host's interrupt handler: runs uACPI's, which reads and clears the
/// events behind the interrupt. Returns whether any was there.
pub fn interrupt() -> bool {
    let Some(Interrupt { handler, ctx }) = *INTERRUPT.lock_irq() else { return false };
    work::interrupt_starts();
    // SAFETY: uACPI installed `handler` with `ctx`, and lets go of neither
    // before `uacpi_kernel_wait_for_work_completion` has waited for this.
    let handled = unsafe { handler(ptr::with_exposed_provenance_mut(ctx)) };
    work::interrupt_ends(host());
    handled == INTERRUPT_HANDLED
}

/// Runs the first piece of work on `queue`, in its thread. Returns false
/// when the queue was empty.
pub fn run_work(queue: Queue) -> bool {
    let Some(Work { handler, ctx }) = work::pop(queue) else { return false };
    // SAFETY: uACPI scheduled `handler` with `ctx` and keeps both valid
    // until the handler has run; nothing touches the entry after.
    unsafe { handler(ptr::with_exposed_provenance_mut(ctx)) };
    work::done(host());
    true
}

// What follows is called by uACPI.

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_get_rsdp(out: *mut u64) -> Status {
    let Some(rsdp) = host().rsdp() else { return NOT_FOUND };
    // SAFETY: uACPI passes where to write the address.
    unsafe { out.write(rsdp) };
    OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_map(phys: u64, len: usize) -> Handle {
    match host().map(phys, len) {
        Some(virt) => ptr::with_exposed_provenance_mut(virt),
        None => MAP_FAILED,
    }
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_unmap(virt: Handle, len: usize) {
    host().unmap(virt.expose_provenance(), len);
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_log(level: u32, message: *const c_char) {
    // SAFETY: uACPI passes a NUL-terminated message.
    let message = unsafe { CStr::from_ptr(message) };
    let level = match level {
        1 => Level::Error,
        2 => Level::Warn,
        3 => Level::Info,
        _ => Level::Debug,
    };
    let text = message.to_str().unwrap_or("(a message that is not UTF-8)");
    host().log(level, format_args!("{}", text.trim_end_matches('\n')));
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_alloc(size: usize) -> Handle {
    allocate(size, false)
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_alloc_zeroed(size: usize) -> Handle {
    allocate(size, true)
}

fn allocate(size: usize, zeroed: bool) -> Handle {
    let Ok(layout) = Layout::from_size_align(size, ALIGN) else { return ptr::null_mut() };
    let heap = host().heap();
    let memory = if zeroed { heap.allocate_zeroed(layout) } else { heap.allocate(layout) };
    memory.map_or(ptr::null_mut(), |memory| memory.as_ptr().cast())
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_free(memory: Handle, size: usize) {
    let Some(memory) = NonNull::new(memory.cast::<u8>()) else { return };
    let Ok(layout) = Layout::from_size_align(size, ALIGN) else { return };
    // SAFETY: uACPI frees only what `uacpi_kernel_alloc` gave it, with the
    // size it asked for (`UACPI_SIZED_FREES`), so with the same layout.
    unsafe { host().heap().deallocate(memory, layout) };
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_get_nanoseconds_since_boot() -> u64 {
    host().now_ns()
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_stall(us: u8) {
    host().stall_us(u32::from(us));
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_sleep(ms: u64) {
    host().sleep_ms(ms);
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_get_thread_id() -> Handle {
    ptr::without_provenance_mut(host().thread_id())
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_disable_interrupts() -> c_ulong {
    c_ulong::from(host().disable_interrupts())
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_restore_interrupts(state: c_ulong) {
    host().restore_interrupts(state != 0);
}

/// Puts `value` on the heap, as a handle.
fn boxed<T>(value: T) -> Handle {
    let heap: &'static (dyn Allocator + Sync) = host().heap();
    match Box::try_new_in(value, heap) {
        Ok(boxed) => Box::into_raw_with_allocator(boxed).0.cast(),
        Err(_) => ptr::null_mut(),
    }
}

/// Frees what [`boxed`] put on the heap.
///
/// # Safety
///
/// `handle` came from `boxed::<T>` and is not used again.
unsafe fn unbox<T>(handle: Handle) {
    if handle.is_null() {
        return;
    }
    let heap: &'static (dyn Allocator + Sync) = host().heap();
    // SAFETY: the caller promises `handle` is a `Box<T>` from `boxed`,
    // made with this heap.
    drop(unsafe { Box::from_raw_in(handle.cast::<T>(), heap) });
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_create_semaphore(units: u32) -> Handle {
    boxed(Semaphore::new(units))
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_free_semaphore(handle: Handle) {
    // SAFETY: uACPI frees a semaphore once, when nothing waits for it.
    unsafe { unbox::<Semaphore>(handle) };
}

/// The semaphore behind a handle.
///
/// # Safety
///
/// `handle` came from `uacpi_kernel_create_semaphore` and is not freed
/// while the reference lives.
unsafe fn semaphore<'a>(handle: Handle) -> &'a Semaphore {
    // SAFETY: as the caller promises.
    unsafe { &*handle.cast::<Semaphore>() }
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_wait_for_semaphore(handle: Handle, timeout_ms: u16) -> Status {
    // SAFETY: uACPI waits only on a semaphore it has not freed.
    let semaphore = unsafe { semaphore(handle) };
    if semaphore.take(host(), timeout_ms) { OK } else { TIMEOUT }
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_signal_semaphore(handle: Handle) {
    // SAFETY: as for waiting.
    unsafe { semaphore(handle) }.give(host());
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_create_spinlock() -> Handle {
    boxed(Spin::new())
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_free_spinlock(handle: Handle) {
    // SAFETY: uACPI frees a spin lock once, when nothing holds it.
    unsafe { unbox::<Spin>(handle) };
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_lock_spinlock(handle: Handle) -> c_ulong {
    // SAFETY: uACPI takes only a spin lock it has not freed.
    let spin = unsafe { &*handle.cast::<Spin>() };
    c_ulong::from(spin.lock(host()))
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_unlock_spinlock(handle: Handle, state: c_ulong) {
    // SAFETY: as for taking it.
    let spin = unsafe { &*handle.cast::<Spin>() };
    spin.unlock(host(), state != 0);
}

/// A PCI function's handle: its number, plus one so that none is null.
pub(crate) fn pci_handle(address: PciAddress) -> Handle {
    let number = (usize::from(address.segment) << 16)
        | (usize::from(address.bus) << 8)
        | (usize::from(address.device) << 3)
        | usize::from(address.function);
    ptr::without_provenance_mut(number + 1)
}

pub(crate) fn pci_address(handle: Handle) -> PciAddress {
    let number = handle.addr().wrapping_sub(1);
    PciAddress {
        segment: (number >> 16) as u16,
        bus: (number >> 8) as u8,
        device: ((number >> 3) & 0x1f) as u8,
        function: (number & 7) as u8,
    }
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_pci_device_open(address: RawPciAddress, out: *mut Handle) -> Status {
    let RawPciAddress { segment, bus, device, function } = address;
    if device >= 32 || function >= 8 {
        return INVALID_ARGUMENT;
    }
    let address = PciAddress { segment, bus, device, function };
    if host().pci_read(address, 0, 2).is_none() {
        // AML finds a missing device by reading all ones; uACPI gives it
        // them for this.
        return NOT_FOUND;
    }
    // SAFETY: uACPI passes where to write the handle.
    unsafe { out.write(pci_handle(address)) };
    OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_pci_device_close(_: Handle) {}

fn pci_read(handle: Handle, offset: usize, width: u8) -> Option<u32> {
    host().pci_read(pci_address(handle), offset, width)
}

fn pci_write(handle: Handle, offset: usize, width: u8, value: u32) -> Status {
    if host().pci_write(pci_address(handle), offset, width, value) { OK } else { INVALID_ARGUMENT }
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_pci_read8(handle: Handle, offset: usize, out: *mut u8) -> Status {
    let Some(value) = pci_read(handle, offset, 1) else { return INVALID_ARGUMENT };
    // SAFETY: uACPI passes where to write the value.
    unsafe { out.write(value as u8) };
    OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_pci_read16(handle: Handle, offset: usize, out: *mut u16) -> Status {
    let Some(value) = pci_read(handle, offset, 2) else { return INVALID_ARGUMENT };
    // SAFETY: uACPI passes where to write the value.
    unsafe { out.write(value as u16) };
    OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_pci_read32(handle: Handle, offset: usize, out: *mut u32) -> Status {
    let Some(value) = pci_read(handle, offset, 4) else { return INVALID_ARGUMENT };
    // SAFETY: uACPI passes where to write the value.
    unsafe { out.write(value) };
    OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_pci_write8(handle: Handle, offset: usize, value: u8) -> Status {
    pci_write(handle, offset, 1, u32::from(value))
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_pci_write16(handle: Handle, offset: usize, value: u16) -> Status {
    pci_write(handle, offset, 2, u32::from(value))
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_pci_write32(handle: Handle, offset: usize, value: u32) -> Status {
    pci_write(handle, offset, 4, value)
}

/// An I/O range's handle: its first port in bits 16 to 31 and its length
/// in bits 32 and up, with bit 0 set so that none is null.
pub(crate) fn io_handle(base: u16, len: usize) -> Handle {
    ptr::without_provenance_mut((len << 32) | (usize::from(base) << 16) | 1)
}

/// The port `offset` bytes into the range, if `width` bytes from there
/// are in it.
pub(crate) fn io_port(handle: Handle, offset: usize, width: usize) -> Option<u16> {
    let (base, len) = ((handle.addr() >> 16) & 0xffff, handle.addr() >> 32);
    let end = offset.checked_add(width)?;
    (end <= len).then(|| (base + offset) as u16)
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_map(base: u64, len: usize, out: *mut Handle) -> Status {
    let Ok(first) = u16::try_from(base) else { return INVALID_ARGUMENT };
    if len == 0 || base + len as u64 > 0x1_0000 {
        return INVALID_ARGUMENT;
    }
    // SAFETY: uACPI passes where to write the handle.
    unsafe { out.write(io_handle(first, len)) };
    OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_unmap(_: Handle) {}

fn io_read(handle: Handle, offset: usize, width: u8) -> Option<u32> {
    let port = io_port(handle, offset, usize::from(width))?;
    Some(host().port_read(port, width))
}

fn io_write(handle: Handle, offset: usize, width: u8, value: u32) -> Status {
    let Some(port) = io_port(handle, offset, usize::from(width)) else { return INVALID_ARGUMENT };
    host().port_write(port, width, value);
    OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_read8(handle: Handle, offset: usize, out: *mut u8) -> Status {
    let Some(value) = io_read(handle, offset, 1) else { return INVALID_ARGUMENT };
    // SAFETY: uACPI passes where to write the value.
    unsafe { out.write(value as u8) };
    OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_read16(handle: Handle, offset: usize, out: *mut u16) -> Status {
    let Some(value) = io_read(handle, offset, 2) else { return INVALID_ARGUMENT };
    // SAFETY: uACPI passes where to write the value.
    unsafe { out.write(value as u16) };
    OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_read32(handle: Handle, offset: usize, out: *mut u32) -> Status {
    let Some(value) = io_read(handle, offset, 4) else { return INVALID_ARGUMENT };
    // SAFETY: uACPI passes where to write the value.
    unsafe { out.write(value) };
    OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_write8(handle: Handle, offset: usize, value: u8) -> Status {
    io_write(handle, offset, 1, u32::from(value))
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_write16(handle: Handle, offset: usize, value: u16) -> Status {
    io_write(handle, offset, 2, u32::from(value))
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_write32(handle: Handle, offset: usize, value: u32) -> Status {
    io_write(handle, offset, 4, value)
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_handle_firmware_request(request: *const FirmwareRequest) -> Status {
    // SAFETY: uACPI passes a request it filled in.
    let request = unsafe { &*request };
    if request.kind == FIRMWARE_REQUEST_FATAL {
        // SAFETY: a `Fatal` request fills the union's `fatal` member.
        let Fatal { kind, code, arg } = unsafe { request.body.fatal };
        host().log(
            Level::Error,
            format_args!(
                "the firmware's AML reports a fatal error: type {kind:#x}, code {code:#x}, \
                 argument {arg:#x}"
            ),
        );
    } else {
        host().log(Level::Debug, format_args!("the firmware's AML reached a breakpoint"));
    }
    OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_install_interrupt_handler(
    irq: u32,
    handler: InterruptHandler,
    ctx: Handle,
    out: *mut Handle,
) -> Status {
    {
        let mut installed = INTERRUPT.lock_irq();
        if installed.is_some() {
            return ALREADY_EXISTS;
        }
        *installed = Some(Interrupt { handler, ctx: ctx.expose_provenance() });
    }
    if !host().start_interrupt(irq) {
        *INTERRUPT.lock_irq() = None;
        return INTERNAL_ERROR;
    }
    // SAFETY: uACPI passes where to write the handle.
    unsafe { out.write(ptr::without_provenance_mut(irq as usize + 1)) };
    OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_uninstall_interrupt_handler(_: InterruptHandler, _: Handle) -> Status {
    host().stop_interrupt();
    *INTERRUPT.lock_irq() = None;
    OK
}

/// The handle of every work item: the queues keep room for each instead.
const WORK_ITEM: Handle = ptr::without_provenance_mut(1);

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_create_work_item() -> Handle {
    let Some(host) = HOST.get() else { return ptr::null_mut() };
    if work::add_item(*host) { WORK_ITEM } else { ptr::null_mut() }
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_free_work_item(_: Handle) {
    work::remove_item();
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_schedule_work(kind: u32, _: Handle, handler: Handler, ctx: Handle) {
    let queue = if kind == WORK_GPE_EXECUTION { Queue::Events } else { Queue::Notify };
    let work = Work { handler, ctx: ctx.expose_provenance() };
    if !work::push(host(), queue, work) {
        host().log(
            Level::Error,
            format_args!("uACPI's work queue had no room for a work item it made"),
        );
    }
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_wait_for_work_completion() -> Status {
    work::wait_idle(host());
    OK
}
