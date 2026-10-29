<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Creating USB install media

`./ob image` builds `out/dukovo-<version>-x86_64.img`. Written to a USB
stick, it boots on UEFI and legacy BIOS computers. Until the installer the
stick holds GRUB and the kernel, which reports the machine's memory,
processors and firmware tables on the screen: useful for checking that a machine
boots Dukovo and what the kernel finds there. The installer arrives in
Phase 14.

## Writing the image

Any USB stick works; writing the image **erases everything on it**.

### With dd on Linux

1. Build the image:

   ```sh
   ./ob image
   ```

2. Plug in the stick and find its device name. `TRAN` shows `usb` for USB
   devices; make sure the size and model match your stick.

   ```sh
   lsblk -d -o NAME,SIZE,MODEL,TRAN
   ```

3. Unmount anything your desktop mounted from it, then write the image,
   replacing `sdX` with the name from step 2:

   ```sh
   sudo umount /dev/sdX?* 2>/dev/null
   sudo dd if=out/dukovo-0.10.3-x86_64.img of=/dev/sdX bs=4M conv=fsync status=progress
   ```

`conv=fsync` makes `dd` wait until the data is on the stick, so it is safe
to unplug when the command returns.

### With a graphical tool

These tools write the same image and check the target is removable:

- **balenaEtcher** on Linux, macOS and Windows: choose "Flash from file".
- **GNOME Disks**: select the stick, open the menu, choose "Restore Disk
  Image".
- **Rufus** on Windows: when asked, choose "Write in DD Image mode".

## Booting from the stick

1. Plug the stick in and switch the computer on.
2. Open the one-time boot menu (F12 on Dell computers) and choose the USB
   stick. It may be listed twice, once for UEFI and once for legacy boot.
3. GRUB shows a menu with one entry, "Dukovo Server 0.10.3 (Radix)", and boots it
   after three seconds.

The screen then shows the kernel's log, under UEFI and legacy BIOS boot
alike, ending with "Boot complete.". The computer then idles; switch it
off with the power button. Where the kernel hears the button, the log
says "Power: the ACPI power button asks Spark to power the machine off",
and a press stops the services and powers the machine off cleanly;
elsewhere the firmware handles the button. If the kernel panics
instead, the screen shows a report; [Debugging the kernel](debugging.md)
explains it, and what to include when you report it.

To include the processor's microcode updates, which Dukovo does not ship
itself, build the image with the build host's copy, for example from
Debian's or Ubuntu's `intel-microcode` package:

```sh
./ob image --microcode /lib/firmware/intel-ucode
```

Machine-specific settings, such as turning Secure Boot off, are in
[Supported hardware](hardware.md).

## Trying the image without a stick

`./ob run` boots the same image in QEMU, attached as a virtual USB stick on
an xHCI controller, which is close to what a real computer sees.
