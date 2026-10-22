<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Supported hardware

Dukovo 1.0 targets 64-bit x86 computers. Two systems are tested on every
change or release:

- **QEMU** (q35 machine, UEFI through OVMF and BIOS through SeaBIOS), in
  every CI run;
- the **Dell Inspiron 15 5570**, the reference laptop, before every
  release.

## Dell Inspiron 15 5570

This is the factory configuration of the reference machine, from Dell's
parts list for its service tag.

| Component | Part | Dukovo support |
| --- | --- | --- |
| Processor | Intel Core i7-8550U (4 cores, 8 threads) | All 8 threads started from Phase 1; early microcode with the optional firmware |
| Memory | 8 GB DDR4-2400, one module | Managed by the frame allocator and kernel heap from Phase 2 (one NUMA node) |
| Graphics | Intel UHD Graphics 620 | Firmware framebuffer console from Phase 1; no native GPU driver in 1.0 |
| SSD | 128 GB SK hynix SC311, M.2 SATA | AHCI driver, Phase 10; Dukovo installs here |
| Hard disk | 1 TB 2.5" SATA, 5400 rpm | AHCI driver, Phase 10; kept for `/data` |
| USB | Intel xHCI | Phase 11 |
| Keyboard | Built-in, PS/2 (i8042) | Phase 9 |
| Touchpad | Likely I2C-HID (Intel Serial IO) | Phase 11, to be confirmed |
| Wired network | Built-in 10/100 Ethernet, likely Realtek | Phase 12, chip to be confirmed |
| Wi-Fi and Bluetooth | Qualcomm QCA9377 (Dell Wireless 1810) | After 1.0; needs closed firmware |
| Audio | Realtek ALC3246 | After 1.0 |
| Serial port | None | Kernel output goes to the screen, and to the crash log for the next boot |

### Firmware settings

Open the firmware setup by pressing **F2** at the Dell logo, and set:

- **System Configuration > SATA Operation: AHCI.** The laptop ships with
  "RAID On" for Intel Rapid Storage, which hides the disks from standard
  AHCI drivers. Windows on the SSD will not boot after this change unless
  it was prepared for it; Dukovo replaces Windows on this machine anyway.
- **Secure Boot > Secure Boot Enable: off.** Dukovo images are not signed
  for Secure Boot before 1.0.
- To test legacy BIOS boot as well, enable **Legacy Option ROMs** and the
  legacy boot list under **General > Boot Sequence**, if your firmware
  version offers them. UEFI is the normal way to boot Dukovo.

Press **F12** at the Dell logo for the one-time boot menu.

### Smoke test

The latest USB image is booted on the laptop at the end of Phases 1, 2, 6
and 9, and of every later phase. Each run does these steps, with the
checks of every phase so far:

1. Build the image, with the laptop's microcode if the build host has
   Intel's (the `intel-microcode` package on Debian and Ubuntu), and write
   it to a USB stick as in [Creating USB install media](usb-media.md):

   ```sh
   ./ob image --microcode /lib/firmware/intel-ucode
   ```

2. Boot it from the F12 menu in UEFI mode. The boot log must appear on the
   screen and end with `Boot complete.` Check, and photograph:
   - `Firmware: UEFI` and a framebuffer of the panel's resolution, drawn
     with the 12x24 font;
   - the memory map, about 8 GB usable;
   - `CPUs: 8 of 8 online`, and the timer and IPI lines saying every CPU
     answered;
   - the `Time:` line (TSC rate, and which timer measured it);
   - the `microcode:` line: updated, or already current;
   - the `APIC:` line: x2APIC mode is expected on this processor.

   From Phase 2, also:
   - the `Random:` line: seeded from RDSEED, RDRAND and timing jitter
     (the i7-8550U has both instructions), with nothing failing its
     health test;
   - the `KASLR:` line: the kernel moved, and a different `Layout:` line
     on each boot;
   - `Paging: W^X holds`, and `SMEP on, SMAP on, PCID on; direct map in
     1 GiB pages` (the i7-8550U has all four);
   - the frame allocator's zones: one node, a `DMA32` zone below 4 GiB and
     a `Normal` zone above, about 7.8 GB free in all.
3. If the firmware offers legacy boot, repeat in BIOS mode.
4. Panic test: boot again with the `selftest=page-fault` flag. At the GRUB
   menu, press an arrow key to stop the countdown, press **e**, add
   ` selftest=page-fault` to the end of the `multiboot2` line, and press
   **Ctrl-X**. (Or build a second image with
   `./ob image --cmdline selftest=page-fault`.) The screen must show the
   panic report with a backtrace naming `radix::selftest::fault`.
5. Press a key to restart, press **F12** again and boot the stick, since
   the restart otherwise boots the internal disk. This boot must start by
   showing the panic of the previous one from the crash log.
6. From Phase 2: boot with `selftest=mm-stress` the same way. With 8
   CPUs it allocates and frees about 50 million objects, and the log must
   end with `selftest mm-stress: passed` before `Boot complete.` Note the
   time it reports.
7. From Phase 3: boot with `selftest=caps` the same way. The log must end
   with `selftest caps: passed` before `Boot complete.`, and the line
   before it must say how many derivations the 7 other CPUs tried during
   the revoke.
8. From Phase 4: check that the first `Scheduler:` line says `idle with
   MWAIT` (the i7-8550U has it) and the second `8 CPUs run threads, on 1
   NUMA node`. Then boot with `selftest=sched-rt`, then with
   `selftest=sched-balance`. Each log must end with `passed` before `Boot
   complete.`; note the timer latency `sched-rt` reports.
9. From Phase 5: check that the `User mode:` line says `SMEP on, SMAP on`.
   Then boot with `selftest=user`, then with `selftest=ipc`. Each log must
   end with `passed` before `Boot complete.` Then boot with
   `selftest=bench` and note the `BENCH` lines, which are the first
   numbers from real hardware.
10. From Phase 6: the log must end with Spark's lines, from `spark: Spark
    v1 is process 1` to `spark: nothing more to do; waiting`, with
    `hello` exiting with code `0x0` and all of its memory coming back.
11. From Phase 7: Spark must say `built the hierarchy of 10 directories
    and mounted 8 filesystems`. Photograph the `PCI:` lines, which list
    every PCI function with its vendor and device IDs and class: they
    name the SATA controller (class `0106xx`) and the Ethernet controller
    (class `0200xx`) that Phases 10 and 12 drive. No `Disk:` line is
    expected, since the laptop has no virtio disks and its SATA drives
    wait for the AHCI driver, so `/volumes` stays empty.

Record the result in the table below, with the commit tested, and open an
issue for anything that fails, with the photos.

| Phase | Date | Commit | UEFI | Legacy BIOS | Notes |
| --- | --- | --- | --- | --- | --- |
| 1 | not yet run | | | | |
| 2 | not yet run | | | | |
| 6 | 2026-10-22 | `daa2682` (Phase 6) | not recorded | not recorded | Chase: "Latest P6 build boots on my laptop without issue". Which firmware mode, and the individual checks above, were not reported. |

### Help wanted: the Ethernet chip

The network driver for Phase 12 depends on the exact Ethernet controller,
which the parts list does not name; the roadmap expects a Realtek RTL810x
(PCI ID `10ec:8136`). From Phase 7, the boot log's `PCI:` lines show it
(class `020000`, Ethernet controller). Or, from any Linux live USB on the
laptop, run:

```sh
lspci -nn | grep -i -E 'ethernet|network'
```

and report the output in an issue on the repository.
