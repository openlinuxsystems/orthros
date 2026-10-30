<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0167. Newer Intel chipsets and AMD's FCH have their watchdog too

- Status: Accepted
- Date: 2026-10-30

## Context

Phase 11d, its fourth part. ADR 0143 had Spark pet the chipset's
hardware watchdog, so that a machine whose Spark stops responding
restarts by itself, and drove the two QEMU has: the i6300esb, and the
TCO timer of the ICH9 and ICH10. It left the reference machines' for
later: the Dell's Kaby Lake PCH moved the TCO timer behind its SMBus
controller and NO_REBOOT behind the sideband interface, and the
Framework's AMD FCH has a watchdog of its own. Neither is in QEMU.

Intel numbers the TCO timer's layouts as versions, and the timer itself
(0.6 s ticks, two stages, TIMEOUT and SECOND_TO_STS) has stayed the same
since the ICH9; what moved is where its registers are and where the
NO_REBOOT bit that must be clear for it to restart the machine is:

- Version 2, from the ICH6 to the 9 Series and the C610: at the LPC
  bridge's ACPI base + 0x60, with NO_REBOOT in the General Control and
  Status register of the root complex registers.
- Version 4, the 100 and 200 Series and the C620 (Sunrise Point and its
  kin, the Dell's among them): the SMBus controller decodes the
  registers at its TCOBASE, and NO_REBOOT is bit 1 of the SMBus's
  General Control register, in the private configuration space behind
  the sideband bridge (P2SB), which firmware hides.
- Version 6, from the 300 Series on: at the SMBus controller's TCOBASE,
  with NO_REBOOT bit 0 of TCO1_CNT.

AMD's FCH watchdog counts down once from a 16-bit count, at a resolution
its power management registers set, and restarts the machine (or powers
it off) when it reaches 0; WatchDogFired says it did. The power
management registers say where it is decoded, at 0xfeb00000 or in the
ACPI MMIO block at 0xfed80b00, and on Zen they are in that block too, at
0xfed80300, since Zen's FCHs may not answer at the old index and data
ports.

FreeBSD's ichwd(4) and amdsbwd(4) (BSD-2-Clause) give these registers;
Intel's patch for Cannon Lake's TCO timer and AMD's for Zen's power
management registers in Linux say where they moved. No code is taken
from any of them.

## Decision

- **The watchdogs are a crate, `radix-watchdog`**, a compartment with no
  `unsafe` code, as the IOMMU engines are. The kernel gives it bus 0's
  configuration space and a `Platform`: device memory mapped if none of
  it is RAM, and I/O ports from 0x400 on, apart from PCI's configuration
  ports, and the FCH's two. The i6300esb's and the ICH9's code moved
  into it unchanged but for one order: the TCO timer is stopped before
  NO_REBOOT is cleared.
- **Intel's TCO timer, versions 2, 4 and 6**, by the LPC bridge's or the
  SMBus controller's device ID:
  - Version 2 for the ICH9 and ICH10, and the 5 to 9 Series, C600 and
    C610 PCHs, as before.
  - Version 4 for the 100 and 200 Series, C620 and Atom C3000. The TCO
    base must be decoded (TCOCTL's TCO_BASE_EN). The sideband bridge is
    shown for as long as reading its SBREG_BAR takes, then hidden again
    if the firmware had hidden it. NO_REBOOT is cleared at the SMBus's
    port (0xc6, or 0xcf on the C3000).
  - Version 6 for the 300 Series to the 800 Series, the Core Ultra
    processors and the server chipsets after the C620. NO_REBOOT is
    cleared in TCO1_CNT.
  - A NO_REBOOT that reads back set (a strap) leaves the timer unused, as
    before.
- **AMD's FCH watchdog**, for the SMBus functions 1022:780b from
  revision 0x41 and 1022:790b from 0x49:
  - On Zen (790b from revision 0x51), the power management registers are
    reached at 0xfed80300, and WatchdogTmrEn, which on Zen also turns
    the watchdog on, is set if firmware left it clear. Before Zen they
    are reached through the ports 0xcd6 and 0xcd7, and WatchdogTmrEn is
    left as it is.
  - The watchdog is used where it is decoded, at 0xfeb00000 or else in
    the ACPI MMIO block. It is set to count seconds, to restart the
    machine rather than power it off, and to be on. One that stays off
    is not used.
  - Timeouts run from 1 to 65535 s, in one stage.
- The names it gets are *the Intel ICH9 TCO timer*, *the Intel ICH10 TCO
  timer*, *the Intel PCH TCO timer* and *the AMD FCH watchdog*.
- The crate's host tests run each kind against a simulated chipset:
  - The TCO timer counts its two stages of 0.6 s ticks.
  - The FCH watchdog counts seconds.
  - The sideband bridge reads all ones while hidden.
  - Each restart sets the bits that keep it from restarting the machine
    again, as firmware leaves them.
  - Each must restart the machine at its timeout and not before, stay
    quiet while petted or stopped, and say so on the next boot. A
    strapped NO_REBOOT, an undecoded or switched-off watchdog, or a
    hidden bridge that is not there leaves it unused.

## Consequences

- The Dell (Sunrise Point-LP, 8086:9d23) and the Framework Desktop (a
  Zen FCH) get the watchdog that ADR 0143 gave QEMU. The `watchdog` and
  `watchdog-tco` boot tests still check the i6300esb and the ICH9 in
  QEMU. The Dell's and the Framework's are checked only by the host tests
  until each machine's run.
- Linux leaves the TCO timer to its ACPI WDAT driver when the firmware
  has a WDAT table; the kernel has no AML yet (Phase 11c), so it uses the
  timer directly, as Linux did before.
- The FCH's index and data ports are shared with the firmware's AML.
  Only `find` uses them, at boot, before any AML runs.
- Not yet: the TCO timers of Intel's Atom SoCs (Bay Trail, Apollo Lake),
  which are behind their power management controllers (versions 3 and
  5), and AMD's SB700 and SB800.
