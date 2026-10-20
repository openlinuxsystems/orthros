# 0009. The Dell Inspiron 15 5570 is the reference machine

- Status: Accepted
- Date: 2026-10-20

## Context

Driver work needs a concrete machine to target and test on before 1.0. The
project owner has a Dell Inspiron 15 5570 (service tag configuration
recorded in the handbook's "Supported hardware" chapter): Intel Core
i7-8550U, Intel UHD Graphics 620, 8 GB DDR4, a 128 GB M.2 SATA SSD with
Windows, a 1 TB SATA hard disk, Qualcomm QCA9377 Wi-Fi, and no serial port.

## Decision

- The Inspiron 15 5570 is the reference machine for 1.0, alongside QEMU.
- Storage drivers start with AHCI (both disks are SATA); NVMe follows.
- OrthrOS installs on the SSD, erasing Windows; the hard disk is left for
  `/data`.
- Firmware settings for OrthrOS are SATA Operation set to AHCI and Secure
  Boot turned off.

## Consequences

- With no serial port, kernel output on this machine needs the framebuffer
  console (Phase 1); in Phase 0 output is visible only when it boots in
  legacy BIOS mode.
- The wired Ethernet controller still has to be confirmed (`lspci -nn` from
  a Linux live USB) before the network driver is chosen in Phase 12.
- Wi-Fi is out of scope for 1.0 (see ADR 0007).
