# Creating USB install media

`./ob image` builds `out/orthros-<version>-x86_64.img`. Written to a USB
stick, it boots on UEFI and legacy BIOS computers. In Phase 0 the stick
holds GRUB and the stub kernel, which is useful for checking that a machine
boots OrthrOS at all; the installer arrives in Phase 14.

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
   sudo dd if=out/orthros-0.0.1-x86_64.img of=/dev/sdX bs=4M conv=fsync status=progress
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
3. GRUB shows a menu with one entry, "OrthrOS 0.0.1 (Radix)", and boots it
   after three seconds.

What you see next depends on the firmware mode in Phase 0:

- **Legacy BIOS boot:** the screen shows the kernel's report, ending with
  "Boot complete.". The computer then stops; switch it off with the power
  button.
- **UEFI boot:** GRUB prints "no suitable video mode found" and the screen
  stays as GRUB left it, because the Phase 0 kernel has no framebuffer
  console yet. The kernel's report goes to the serial port, which laptops
  usually lack. Phase 1 adds the framebuffer console.

Machine-specific settings, such as turning Secure Boot off, are in
[Supported hardware](hardware.md).

## Trying the image without a stick

`./ob run` boots the same image in QEMU, attached as a virtual USB stick on
an xHCI controller, which is close to what a real computer sees.
