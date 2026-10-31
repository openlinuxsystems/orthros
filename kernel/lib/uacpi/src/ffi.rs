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
use crate::{HOST, Host, Level, PciAddress, Queue, SleepState, ec, host, power};

type Status = u32;
type Handle = *mut c_void;
type InterruptHandler = unsafe extern "C" fn(Handle) -> u32;
type IterationCallback = extern "C" fn(Handle, Handle, u32) -> u32;
type NotifyHandler = extern "C" fn(Handle, Handle, u64) -> Status;
type RegionHandler = extern "C" fn(u32, Handle) -> Status;
type GpeHandler = extern "C" fn(Handle, Handle, u16) -> u32;

const OK: Status = 0;
const NOT_FOUND: Status = 6;
const INVALID_ARGUMENT: Status = 7;
const ALREADY_EXISTS: Status = 9;
const INTERNAL_ERROR: Status = 10;
const HARDWARE_TIMEOUT: Status = 17;
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
/// `uacpi_iteration_decision`: go on.
const ITERATION_CONTINUE: u32 = 0;
/// `uacpi_object_type_bits`: thermal zones.
const THERMAL_ZONE_BIT: u32 = 1 << 13;
/// `UACPI_MAX_DEPTH_ANY`.
const MAX_DEPTH_ANY: u32 = u32::MAX;
/// `uacpi_address_space`: the embedded controller's.
const ADDRESS_SPACE_EMBEDDED_CONTROLLER: u32 = 3;
/// `uacpi_region_op`: a read, then a write.
const REGION_OP_READ: u32 = 2;
const REGION_OP_WRITE: u32 = 3;
/// `uacpi_gpe_triggering`: edge-triggered.
const GPE_TRIGGERING_EDGE: u32 = 1;
/// Returned by a GPE's handler: turn the event back on.
const GPE_REENABLE: u32 = 1 << 7;
/// How long to wait for ACPI's global lock, in milliseconds: for good.
const GLOBAL_LOCK_FOREVER: u16 = 0xffff;

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
    fn uacpi_eval_simple(parent: Handle, path: *const c_char, out: *mut Handle) -> Status;
    fn uacpi_object_unref(object: Handle);
    fn uacpi_object_get_integer(object: Handle, out: *mut u64) -> Status;
    fn uacpi_object_get_string_or_buffer(object: Handle, out: *mut DataView) -> Status;
    fn uacpi_object_get_package(object: Handle, out: *mut ObjectArray) -> Status;
    fn uacpi_find_devices(hid: *const c_char, callback: IterationCallback, user: Handle) -> Status;
    fn uacpi_namespace_root() -> Handle;
    fn uacpi_namespace_for_each_child(
        parent: Handle,
        descending: Option<IterationCallback>,
        ascending: Option<IterationCallback>,
        types: u32,
        max_depth: u32,
        user: Handle,
    ) -> Status;
    fn uacpi_namespace_node_name(node: Handle) -> u32;
    fn uacpi_namespace_node_generate_absolute_path(node: Handle) -> *const c_char;
    fn uacpi_free_absolute_path(path: *const c_char);
    fn uacpi_install_notify_handler(node: Handle, handler: NotifyHandler, ctx: Handle) -> Status;
    fn uacpi_install_address_space_handler(
        node: Handle,
        space: u32,
        handler: RegionHandler,
        ctx: Handle,
    ) -> Status;
    fn uacpi_install_gpe_handler(
        device: Handle,
        index: u16,
        triggering: u32,
        handler: GpeHandler,
        ctx: Handle,
    ) -> Status;
    fn uacpi_enable_gpe(device: Handle, index: u16) -> Status;
    fn uacpi_acquire_global_lock(timeout: u16, sequence: *mut u32) -> Status;
    fn uacpi_release_global_lock(sequence: u32) -> Status;
    fn uacpi_status_to_string(status: Status) -> *const c_char;
}

/// `uacpi_data_view`: a string's or a buffer's bytes.
#[repr(C)]
struct DataView {
    bytes: *const u8,
    len: usize,
}

/// `uacpi_object_array`: a package's elements.
#[repr(C)]
struct ObjectArray {
    objects: *const Handle,
    count: usize,
}

/// `uacpi_region_rw_data`: a read or write of a region's.
#[repr(C)]
struct RegionAccess {
    handler_context: Handle,
    region_context: Handle,
    address: u64,
    value: u64,
    width: u8,
}

/// One of uACPI's errors: a `uacpi_status` other than success.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error(Status);

impl Error {
    /// The `uacpi_status` number.
    pub fn code(self) -> u32 {
        self.0
    }

    /// Whether what was asked for is not in the namespace.
    pub fn is_not_found(self) -> bool {
        self.0 == NOT_FOUND
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
    }
    // Before `_STA` and `_INI`, which often read it.
    ec::probe();
    // SAFETY: as above.
    check(unsafe { uacpi_namespace_initialize() }).map_err(fail("initialize the namespace"))
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

/// A device of the namespace, as [`find_devices`] finds it. The kernel
/// never unloads a table, so a node stays for good.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Node(NonNull<c_void>);

// SAFETY: a node only names one of uACPI's, which locks its namespace
// itself.
unsafe impl Send for Node {}
// SAFETY: as for `Send`.
unsafe impl Sync for Node {}

/// An object AML returned, given back to uACPI when dropped.
struct Object(NonNull<c_void>);

impl Drop for Object {
    fn drop(&mut self) {
        // SAFETY: uACPI handed over this reference to the object.
        unsafe { uacpi_object_unref(self.0.as_ptr()) };
    }
}

/// Calls `found` with each device present whose `_HID` or `_CID` is
/// `hid`, as `PNP0C0A`. From a thread that may sleep, after [`start`]
/// has loaded the namespace.
pub fn find_devices(hid: &CStr, found: &mut dyn FnMut(Node)) {
    extern "C" fn each(user: Handle, node: Handle, _: u32) -> u32 {
        // SAFETY: `user` is the `&mut dyn FnMut` below, alive while
        // `uacpi_find_devices` runs.
        let found = unsafe { &mut *user.cast::<&mut dyn FnMut(Node)>() };
        if let Some(node) = NonNull::new(node) {
            found(Node(node));
        }
        ITERATION_CONTINUE
    }
    let mut found = found;
    // SAFETY: `hid` is NUL-terminated; `each` is handed `found` only.
    let status = unsafe { uacpi_find_devices(hid.as_ptr(), each, (&raw mut found).cast()) };
    if status != OK {
        host().log(
            Level::Warn,
            format_args!("could not look for {hid:?} devices: {}", Error(status)),
        );
    }
}

/// Calls `found` with each thermal zone of the namespace, in the order
/// uACPI walks it.
pub fn find_thermal_zones(found: &mut dyn FnMut(Node)) {
    extern "C" fn each(user: Handle, node: Handle, _: u32) -> u32 {
        // SAFETY: `user` is the `&mut dyn FnMut` below, alive while
        // `uacpi_namespace_for_each_child` runs.
        let found = unsafe { &mut *user.cast::<&mut dyn FnMut(Node)>() };
        if let Some(node) = NonNull::new(node) {
            found(Node(node));
        }
        ITERATION_CONTINUE
    }
    let mut found = found;
    // SAFETY: the root is uACPI's; `each` is handed `found` only.
    let status = unsafe {
        uacpi_namespace_for_each_child(
            uacpi_namespace_root(),
            Some(each),
            None,
            THERMAL_ZONE_BIT,
            MAX_DEPTH_ANY,
            (&raw mut found).cast(),
        )
    };
    if status != OK {
        host()
            .log(Level::Warn, format_args!("could not look for thermal zones: {}", Error(status)));
    }
}

impl Node {
    /// Its name, as `BAT0`: four characters, padded with `_`.
    pub fn name(self) -> [u8; 4] {
        // SAFETY: a node uACPI gave; the name's union is its text as a
        // little-endian number.
        unsafe { uacpi_namespace_node_name(self.0.as_ptr()) }.to_le_bytes()
    }

    /// Its name as text, as `BAT0`.
    pub fn name_text(self) -> NodeName {
        NodeName(self.name())
    }

    /// Its absolute path, as `\_SB.PCI0.LPCB.EC0`, for the log.
    pub fn path(self) -> NodePath {
        NodePath(self)
    }

    fn eval(self, name: &CStr) -> Result<Option<Object>, Error> {
        let mut object = ptr::null_mut();
        // SAFETY: `name` is NUL-terminated and relative to the node; uACPI
        // writes `object` only on success, with a reference we then own.
        check(unsafe { uacpi_eval_simple(self.0.as_ptr(), name.as_ptr(), &raw mut object) })?;
        Ok(NonNull::new(object).map(Object))
    }

    /// Evaluates `name`, an object or method of the node (as `_STA`), as
    /// an integer. From a thread that may sleep.
    pub fn integer(self, name: &CStr) -> Result<u64, Error> {
        let mut value = 0;
        // SAFETY: as in `eval`; `value` is written on success.
        check(unsafe {
            uacpi_eval_simple_integer(self.0.as_ptr(), name.as_ptr(), &raw mut value)
        })?;
        Ok(value)
    }

    /// Evaluates `name` and hands `read` the package it returns. From a
    /// thread that may sleep.
    pub fn package<T>(self, name: &CStr, read: impl FnOnce(&Package<'_>) -> T) -> Result<T, Error> {
        let object = self.eval(name)?.ok_or(Error(INVALID_ARGUMENT))?;
        let mut array = ObjectArray { objects: ptr::null(), count: 0 };
        // SAFETY: `object` is alive; uACPI fills in `array` on success.
        check(unsafe { uacpi_object_get_package(object.0.as_ptr(), &raw mut array) })?;
        let objects = if array.count == 0 {
            &[][..]
        } else {
            // SAFETY: the package's elements, which live as long as it.
            unsafe { core::slice::from_raw_parts(array.objects, array.count) }
        };
        Ok(read(&Package { objects }))
    }

    /// Evaluates `name` and hands `read` the bytes of the buffer or the
    /// string it returns. From a thread that may sleep.
    pub fn bytes<T>(self, name: &CStr, read: impl FnOnce(&[u8]) -> T) -> Result<T, Error> {
        let object = self.eval(name)?.ok_or(Error(INVALID_ARGUMENT))?;
        Ok(read(view(object.0.as_ptr()).ok_or(Error(INVALID_ARGUMENT))?))
    }

    /// Evaluates `name` for what it does; what it returns is dropped.
    /// From a thread that may sleep.
    pub fn run(self, name: &CStr) -> Result<(), Error> {
        self.eval(name).map(drop)
    }

    /// Calls `handler` in the notify thread with each value AML's
    /// `Notify` sends the node. Called once a node, from a thread that may
    /// sleep.
    pub fn on_notify(self, handler: fn(Node, u64)) -> Result<(), Error> {
        extern "C" fn notify(ctx: Handle, node: Handle, value: u64) -> Status {
            // SAFETY: `ctx` is the `fn` installed below, as an address.
            let handler: fn(Node, u64) = unsafe { core::mem::transmute(ctx) };
            if let Some(node) = NonNull::new(node) {
                handler(Node(node), value);
            }
            OK
        }
        let ctx: Handle = (handler as *const ()).cast_mut().cast();
        // SAFETY: `notify` turns `ctx` back into the `fn` it is.
        check(unsafe { uacpi_install_notify_handler(self.0.as_ptr(), notify, ctx) })
    }
}

/// The bytes of a string or a buffer object, without a string's ending
/// NUL; `None` for any other object.
fn view<'a>(object: Handle) -> Option<&'a [u8]> {
    let mut view = DataView { bytes: ptr::null(), len: 0 };
    // SAFETY: an object uACPI gave; it fills in `view` on success.
    if unsafe { uacpi_object_get_string_or_buffer(object, &raw mut view) } != OK {
        return None;
    }
    if view.len == 0 || view.bytes.is_null() {
        return Some(&[]);
    }
    // SAFETY: the object's bytes, alive as long as it is.
    let bytes = unsafe { core::slice::from_raw_parts(view.bytes, view.len) };
    Some(bytes.strip_suffix(&[0]).unwrap_or(bytes))
}

/// A node's name as text, as `BAT0`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct NodeName([u8; 4]);

impl NodeName {
    /// The name, `?` where it is not ASCII.
    pub fn as_str(&self) -> &str {
        if self.0.iter().all(u8::is_ascii_graphic) {
            core::str::from_utf8(&self.0).unwrap_or("????")
        } else {
            "????"
        }
    }
}

impl fmt::Display for NodeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A node's absolute path, written when shown.
pub struct NodePath(Node);

impl fmt::Display for NodePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: a node uACPI gave; the path it makes is freed below.
        let path = unsafe { uacpi_namespace_node_generate_absolute_path(self.0.0.as_ptr()) };
        if path.is_null() {
            return f.write_str("?");
        }
        // SAFETY: a NUL-terminated string uACPI made.
        let text = unsafe { CStr::from_ptr(path) }.to_str().unwrap_or("?");
        let written = f.write_str(text);
        // SAFETY: made by `uacpi_namespace_node_generate_absolute_path`.
        unsafe { uacpi_free_absolute_path(path) };
        written
    }
}

/// The elements of a package AML returned, while it lives.
pub struct Package<'a> {
    objects: &'a [Handle],
}

impl Package<'_> {
    /// How many elements it has.
    pub fn len(&self) -> usize {
        self.objects.len()
    }

    /// Whether it has none.
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }
}

impl power::Fields for Package<'_> {
    fn count(&self) -> usize {
        self.len()
    }

    fn integer(&self, index: usize) -> Option<u64> {
        let object = *self.objects.get(index)?;
        let mut value = 0;
        // SAFETY: an element of the package, alive as long as it is.
        (unsafe { uacpi_object_get_integer(object, &raw mut value) } == OK).then_some(value)
    }

    fn text(&self, index: usize) -> Option<&[u8]> {
        view(*self.objects.get(index)?)
    }
}

/// Has the embedded controller's regions under `node` go through
/// [`ec::read`] and [`ec::write`]; uACPI runs their `_REG`.
pub(crate) fn serve_embedded_controller(node: Node) -> Result<(), Error> {
    extern "C" fn region(op: u32, data: Handle) -> Status {
        if op != REGION_OP_READ && op != REGION_OP_WRITE {
            // Attaching and detaching a region need nothing.
            return OK;
        }
        // SAFETY: for a read or a write, uACPI passes its access.
        let access = unsafe { &mut *data.cast::<RegionAccess>() };
        let result = if op == REGION_OP_READ {
            ec::read(access.address, access.width).map(|value| access.value = value)
        } else {
            ec::write(access.address, access.width, access.value)
        };
        match result {
            Ok(()) => OK,
            Err(ec::Access::Timeout) => HARDWARE_TIMEOUT,
            Err(ec::Access::Missing | ec::Access::Range) => INVALID_ARGUMENT,
        }
    }
    // SAFETY: `region` needs no context and lives for good.
    check(unsafe {
        uacpi_install_address_space_handler(
            node.0.as_ptr(),
            ADDRESS_SPACE_EMBEDDED_CONTROLLER,
            region,
            ptr::null_mut(),
        )
    })
}

/// Calls `handler`, in an interrupt handler, each time general-purpose
/// event `index` of `\_GPE` fires (edge-triggered), and turns it on.
pub(crate) fn on_gpe(index: u16, handler: fn()) -> Result<(), Error> {
    extern "C" fn gpe(ctx: Handle, _: Handle, _: u16) -> u32 {
        // SAFETY: `ctx` is the `fn` installed below, as an address.
        let handler: fn() = unsafe { core::mem::transmute(ctx) };
        handler();
        INTERRUPT_HANDLED | GPE_REENABLE
    }
    let ctx: Handle = (handler as *const ()).cast_mut().cast();
    // SAFETY: a null device is `\_GPE`; `gpe` turns `ctx` back into the
    // `fn` it is.
    unsafe {
        check(uacpi_install_gpe_handler(ptr::null_mut(), index, GPE_TRIGGERING_EDGE, gpe, ctx))?;
        check(uacpi_enable_gpe(ptr::null_mut(), index))
    }
}

/// Takes ACPI's global lock, which the firmware's System Management Mode
/// code shares, waiting as long as it takes. Returns what
/// [`give_global_lock`] needs, or `None` if it could not.
pub(crate) fn take_global_lock() -> Option<u32> {
    let mut sequence = 0;
    // SAFETY: uACPI writes `sequence` on success.
    let status = unsafe { uacpi_acquire_global_lock(GLOBAL_LOCK_FOREVER, &raw mut sequence) };
    (status == OK).then_some(sequence)
}

/// Gives back the global lock [`take_global_lock`] took.
pub(crate) fn give_global_lock(sequence: u32) {
    // SAFETY: `sequence` is what `uacpi_acquire_global_lock` gave.
    let _ = unsafe { uacpi_release_global_lock(sequence) };
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
    if queue == Queue::Events && ec::run_queries() {
        return true;
    }
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
