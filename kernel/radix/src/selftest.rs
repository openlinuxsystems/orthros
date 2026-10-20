//! Self-tests chosen with `selftest=name`, which `ob test` boots to check
//! the paths that only run when something goes wrong.
//!
//! - `page-fault` touches an unmapped address. The page fault must panic
//!   with a register dump and a backtrace that names this module.
//! - `crash-reboot` panics, and after the restart checks that the crash
//!   log kept the panic report. It needs a restart that keeps RAM, which is
//!   why `ob test` runs it without QEMU's `-no-reboot`.
//! - `breakpoint` runs a breakpoint instruction: `kdb` takes it when booted
//!   with the `kdb` flag, otherwise it is logged and skipped.

use radix_hal::cmdline::SELFTESTS;
use radix_hal::{Cpu, Platform};
use radix_klog::crashlog::Ending;
use radix_klog::{error, info, warn};

use crate::panic::SELFTEST_MARKER;
use crate::{Arch, crashlog};

/// Runs the self-test called `name`.
pub fn run(name: &str) {
    match name {
        "page-fault" => page_fault(),
        "crash-reboot" => crash_reboot(),
        "breakpoint" => breakpoint(),
        _ => warn!("selftest: no test called `{name}`; the tests are {SELFTESTS:?}"),
    }
}

#[inline(never)]
fn page_fault() {
    info!("selftest page-fault: touching an unmapped address; a page-fault panic follows");
    Arch::self_test_fault();
}

#[inline(never)]
fn crash_reboot() {
    if !crashlog::active() {
        error!("selftest crash-reboot: failed; there is no crash log on this boot");
        return;
    }
    match crashlog::previous(SELFTEST_MARKER) {
        Some((Ending::Panicked, true)) => {
            info!(
                "selftest crash-reboot: passed; the previous boot's panic report survived the restart"
            );
        }
        Some((ending, true)) => {
            error!(
                "selftest crash-reboot: failed; the previous boot panicked as planned, but its log \
                 says it {ending}"
            );
        }
        _ => {
            info!("selftest crash-reboot: panicking now; the next boot checks the crash log");
            panic!("{SELFTEST_MARKER}");
        }
    }
}

#[inline(never)]
fn breakpoint() {
    info!("selftest breakpoint: running a breakpoint instruction");
    Arch::breakpoint();
    info!("selftest breakpoint: passed; execution continued after the breakpoint");
}
