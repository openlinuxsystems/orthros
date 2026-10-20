# Supported hardware

OrthrOS 1.0 targets 64-bit x86 computers. Two systems are tested on every
change or release:

- **QEMU** (q35 machine, UEFI through OVMF and BIOS through SeaBIOS), in
  every CI run;
- the **Dell Inspiron 15 5570**, the reference laptop, before every
  release.

## Dell Inspiron 15 5570

This is the factory configuration of the reference machine, from Dell's
parts list for its service tag.

| Component | Part | OrthrOS support |
| --- | --- | --- |
| Processor | Intel Core i7-8550U (4 cores, 8 threads) | Boots in Phase 0; all cores from Phase 1 |
| Memory | 8 GB DDR4-2400, one module | Phase 2 |
| Graphics | Intel UHD Graphics 620 | Firmware framebuffer from Phase 1; no native GPU driver in 1.0 |
| SSD | 128 GB SK hynix SC311, M.2 SATA | AHCI driver, Phase 10; OrthrOS installs here |
| Hard disk | 1 TB 2.5" SATA, 5400 rpm | AHCI driver, Phase 10; kept for `/data` |
| USB | Intel xHCI | Phase 11 |
| Keyboard | Built-in, PS/2 (i8042) | Phase 11 |
| Touchpad | Likely I2C-HID (Intel Serial IO) | Phase 11, to be confirmed |
| Wired network | Built-in 10/100 Ethernet, likely Realtek | Phase 12, chip to be confirmed |
| Wi-Fi and Bluetooth | Qualcomm QCA9377 (Dell Wireless 1810) | After 1.0; needs closed firmware |
| Audio | Realtek ALC3246 | After 1.0 |
| Serial port | None | Kernel output needs the screen |

### Firmware settings

Open the firmware setup by pressing **F2** at the Dell logo, and set:

- **System Configuration > SATA Operation: AHCI.** The laptop ships with
  "RAID On" for Intel Rapid Storage, which hides the disks from standard
  AHCI drivers. Windows on the SSD will not boot after this change unless
  it was prepared for it; OrthrOS replaces Windows on this machine anyway.
- **Secure Boot > Secure Boot Enable: off.** OrthrOS images are not signed
  for Secure Boot before 1.0.
- To try legacy BIOS boot (the only mode with visible output in Phase 0),
  enable **Legacy Option ROMs** and the legacy boot list under **General >
  Boot Sequence**, if your firmware version offers them.

Press **F12** at the Dell logo for the one-time boot menu.

### Help wanted: the Ethernet chip

The network driver for Phase 12 depends on the exact Ethernet controller,
which the parts list does not name. From any Linux live USB on the laptop,
run:

```sh
lspci -nn | grep -i -E 'ethernet|network'
```

and report the output in an issue on the repository.
