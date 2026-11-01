radix-sysfs(5) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

radix-sysfs - the kernel's view of the hardware and its settings, one
value per file in /kernel

# DESCRIPTION

A *sys* filesystem, which Spark mounts at _/kernel_ (*spark*(8)), with
_/sys_, Linux's name for it, a link to it (*radix-files*(7)), holds the
kernel's view of the hardware and its settings, one value per file, each
ending in a newline. It is read-only. Every file is written when it is
opened, and its size in *stat* is 0 until then, so read a file to its end
rather than by its size.

Wherever Linux has the same thing, the path beneath the folder and the
format are Linux's, so that ported tools find it where they look through
the link: *lspci* reads _/sys/bus/pci/devices_, *lsblk* reads
_/sys/block_. The files at the top, _version_, _cmdline_ and _cpus_, are
Radix's own.

This page describes _/kernel_ as of Phase 11 of the roadmap; later phases
add devices and settings, and settings that can be changed.

# FILES

_/kernel/version_
	The kernel's name and version, and the Dukovo release it is part
	of: *Radix 0.10.4 (Dukovo Server 0.10.4)*. The C library gives the
	second word as the release, and the whole line as the version, in
	uname(3)'s *struct utsname*.

_/kernel/cmdline_
	The kernel command line the boot loader gave (*dukovo-boot*(7)).

_/kernel/cpus_
	The CPUs online.

_/kernel/block/_*name*_/size_
	The size of a disk or partition in 512-byte sectors, whatever its
	block size, as on Linux.

_/kernel/block/_*name*_/ro_
	*1* if the device is read-only, *0* if not.

_/kernel/block/_*name*_/partition_, _start_
	For a partition: its number in its disk's table, the one its name
	ends in, and its first sector on the disk, in 512-byte sectors. A
	partition has these instead of a _queue_ directory, as on Linux,
	whose tools read the queue of the partition's disk.

_/kernel/block/_*name*_/parttype_
	For a partition: its type, as *lsblk*(8)'s PARTTYPE and *blkid*(8)
	give it: its MBR type byte in hexadecimal (*0x0c*, *0xef*), or its
	GPT type's GUID in lowercase
	(*c12a7328-f81f-11d2-ba4b-00a0c93ec93b* for an EFI system
	partition). Linux has it only through udev; Radix has it here.

_/kernel/block/_*name*_/device/model_, _serial_, _firmware_rev_
	For a SATA, NVMe or USB disk or an SD card: its model, serial number and firmware
	revision, as it gives them, without the padding. A USB disk's model
	is its SCSI vendor and product, its firmware its SCSI revision, and
	its serial number its USB device's, as udev's *ID_SERIAL_SHORT* has
	it, up to 64 bytes. An SD card's model is its product's name, its
	serial number the CID's in hex (_0xdeadbeef_) and its firmware the
	CID's revision (_1.0_), which Linux has as _name_, _serial_ and
	_prv_. Linux has these for
	an NVMe disk; for a SATA disk it cuts the model to 16 characters and
	keeps the serial number elsewhere. A tool that must write to one
	particular disk should find it by its serial number, since disk names
	follow the order the controllers are found in.

_/kernel/block/_*name*_/queue/logical_block_size_, _physical_block_size_
	The sizes in bytes of the device's logical blocks, the least it
	reads or writes (*512*, or *4096* for a disk with 4 KiB sectors),
	and of the blocks it writes without reading first (*radix-block*(2)).
	Offsets and sizes elsewhere stay in 512-byte sectors, as on Linux.

_/kernel/block/_*name*_/queue/minimum_io_size_, _optimal_io_size_
	The smallest write it prefers, its physical block size, and the
	size it does best, *0* as it does not say.

_/kernel/block/_*name*_/queue/rotational_
	*1* for a hard disk, or a disk that does not say; *0* for a solid
	state disk.

_/kernel/block/_*name*_/queue/discard_granularity_, _discard_max_bytes_
	Its logical block size, and the most bytes one discard (TRIM)
	covers, if the device discards; *0* and *0* if not.

_/kernel/block/_*name*_/stat_
	For a disk: what it has done since it was found, as the 17 numbers
	of Linux's _/sys/block/_*name*_/stat_, in its order and widths:
	reads, reads merged, sectors read, milliseconds reading; the same
	four of writes; requests in flight, milliseconds with one or more in
	flight, and milliseconds of every request added up; the same four of
	discards; flushes and milliseconds flushing. Sectors are of 512
	bytes. Radix merges no requests, so those numbers are *0*; a call
	that moves many buffers at once counts as one request. *iostat*(1)
	reads it.

	Disks and partitions are named as Linux names them: _vda_, _vdb_ for
	virtio disks, _sda_, _sdb_ for SATA and USB disks, _nvme0n1_ for the first
	namespace of the first NVMe controller, _mmcblk0_ for the first SD
	card, _vda1_ for the first partition of _vda_ and _nvme0n1p1_ for the
	first of _nvme0n1_. Linux puts a
	partition's directory inside its disk's (_/sys/block/vda/vda1_) and
	lists both in _/sys/class/block_; Radix lists both here.

_/kernel/bus/pci/devices/_*address*_/vendor_, _device_
	The PCI function's vendor and device IDs, in hexadecimal with four
	digits: *0x1af4* and *0x1001* for a virtio block device as QEMU
	offers it (*0x1042* when it has no legacy interface).

_/kernel/bus/pci/devices/_*address*_/class_
	Its class code, the base class, subclass and programming interface,
	in hexadecimal with six digits: *0x010000* for a SCSI-like storage
	controller, *0x010601* for an AHCI one, *0x010802* for an NVMe one.

_/kernel/bus/pci/devices/_*address*_/revision_
	Its revision, in hexadecimal with two digits: *0x02*.

_/kernel/bus/pci/devices/_*address*_/subsystem_vendor_, _subsystem_device_
	The vendor and model of the board around it, in hexadecimal with
	four digits: *0x1af4* and *0x1100* on QEMU. A bridge's are in its
	Subsystem ID capability; *0x0000* for a function that has none.

	A function's address is its PCI segment, bus, device and function,
	as Linux writes it: _0000:00:03.0_. Every function the kernel's scan
	found is listed, whether or not a driver uses it. *lspci*(8) reads
	them.

_/kernel/bus/usb/devices/_*name*_/idVendor_, _idProduct_, _bcdDevice_, _version_
	A USB device's vendor and product IDs and its release, in
	hexadecimal with four digits, and the USB version it speaks, as
	Linux writes it, with a space in front: *046d*, *c52b*, *1211* and
	*2.00*. Its device
	descriptor's other fields are in _bDeviceClass_, _bDeviceSubClass_,
	_bDeviceProtocol_ (two hexadecimal digits), _bMaxPacketSize0_ and
	_bNumConfigurations_; its configuration's in _bConfigurationValue_,
	_bNumInterfaces_, _bmAttributes_ and _bMaxPower_ (*100mA*).

	A device's _name_ is its bus and its ports from the root hub down, as
	Linux names it: _1-3.2_ for port 2 of the hub on root port 3 of bus 1.
	Each controller is a bus, numbered from 1 as the kernel finds them.

_/kernel/bus/usb/devices/_*name*_/busnum_, _devnum_, _devpath_, _speed_, _maxchild_
	Its bus; its number on the bus, from 2, each device set up taking
	the next one free, as on Linux; its ports, _3.2_; its speed in Mb/s,
	*1.5*, *12*, *480*, *5000* or *10000*; and a hub's ports, else *0*.

_/kernel/bus/usb/devices/_*name*_/manufacturer_, _product_, _serial_
	Its strings, read when it was set up, for those it has.

_/kernel/bus/usb/devices/_*name*:*c*.*i*_/bInterfaceNumber_, _bInterfaceClass_, _driver_
	Interface _i_ of its configuration _c_: its descriptor's fields, as
	Linux shows them (_bInterfaceNumber_, _bAlternateSetting_,
	_bNumEndpoints_, _bInterfaceClass_, _bInterfaceSubClass_,
	_bInterfaceProtocol_), and _driver_, a link to the driver that took
	it in _/kernel/bus/usb/drivers_: *hub*, *usbhid*, *usb-storage* or
	*uas*.

_/kernel/bus/usb/devices/usb_*n*
	Bus _n_'s root hub: _busnum_; _devnum_ *1*; _devpath_ *0*; _speed_ and
	_version_, *5000* and *3.00* when some of its ports are USB 3's;
	_maxchild_, the controller's root ports; _bDeviceClass_ *09*;
	_product_, *xHCI Host Controller*; and _serial_, the controller's
	PCI address. Linux gives a root hub the Linux Foundation's IDs;
	Radix gives it none. *lsusb*(8) reads all of these (ADR 0230).

_/kernel/bus/i2c/devices/i2c-0/name_
	When the kernel drives the chipset's SMBus controller (ADR 0206),
	its adapter's name, as Linux gives it: *SMBus I801 adapter at 0700*
	for Intel's, with its I/O port in hexadecimal, or *SMBus PIIX4
	adapter port 0 at 0b00* for that of AMD's FCH.

_/kernel/bus/i2c/devices/0-00_*nn*_/name_, _eeprom_
	A memory module's SPD EEPROM on the SMBus, at address *nn*
	(*50* to *57*), found as Linux finds them, by the memory's kind in
	the SMBIOS tables (ADR 0232): _name_ is the kind of EEPROM, as Linux
	names its driver, *spd* (DDR3 and older), *ee1004* (DDR4) or
	*spd5118* (DDR5); _eeprom_ is its bytes, 256, 512 or 1024 of them,
	read from the module the first time the file is opened, which takes
	up to a second, and kept. A read that fails is an I/O error.
	*hwinfo*(8) decodes them.

_/kernel/bus/i2c/devices/i2c-_*n*_/name_
	Each I2C bus of a DesignWare controller, Intel LPSS's or AMD's,
	numbered from *1* in the order the kernel found them (ADR 0234):
	*Synopsys DesignWare I2C adapter*, as Linux names it.

_/kernel/bus/i2c/devices/i2c-_*HID*:*nn*_/name_
	Each device the firmware names on one of those buses, by its ACPI
	name, as Linux names it: its _\_HID_ and its place among the devices
	of that _\_HID_ on the buses, in hexadecimal, as *PNP0C50:00*.

_/kernel/firmware/memmap/_*n*_/start_, _end_, _type_
	The firmware's memory map, as the boot loader gave it to the kernel,
	a directory a range numbered from *0* in its order, as Linux's
	_/sys/firmware/memmap_: the range's first and last byte, in
	hexadecimal (*0x0* and *0x9fbff*), and its type, in Linux's names:
	*System RAM*, *Reserved*, *ACPI Tables*, *ACPI Non-volatile Storage*
	or *Unusable memory*. *lsmem*(1) reads it.

_/kernel/class/dmi/id/sys_vendor_, _product_name_, _product_version_, _product_family_, _product_sku_
	Where the firmware has SMBIOS tables (ADR 0231), what it calls the
	machine: who made it, its product name, version, family and SKU, as
	Linux's _/sys/class/dmi/id_ has them. A file the firmware gives no
	string for is not there. The machine's serial number and UUID, which
	Linux shows to root alone, are not here: *hwinfo*(8) reads them
	through the platform capability (*radix-platform*(2)).

_/kernel/class/dmi/id/board_vendor_, _board_name_, _board_version_, _board_asset_tag_
	Its main board's maker, name, version and asset tag.

_/kernel/class/dmi/id/chassis_type_, _chassis_vendor_, _chassis_version_, _chassis_asset_tag_
	Its chassis's type, a number of the specification's (DSP0134's
	7.4.1: *3* a desktop, *10* a notebook), maker, version and asset tag.

_/kernel/class/dmi/id/bios_vendor_, _bios_version_, _bios_date_, _bios_release_, _ec_firmware_release_
	Its firmware's vendor, version and date (_MM/DD/YYYY_), and the
	releases of the firmware and of the embedded controller's, as
	_major.minor_.

_/kernel/class/watchdog/watchdog0/identity_
	The hardware watchdog the kernel drives, if any (*spark*(8)'s
	*WATCHDOG*): _the Intel 6300ESB watchdog_, _the Intel ICH9 TCO
	timer_, _the Intel ICH10 TCO timer_, _the Intel PCH TCO timer_ or _the
	AMD FCH watchdog_. Without one, a battery or AC adapter, or SMBIOS
	tables, _/kernel/class_ is not there.

_/kernel/class/watchdog/watchdog0/state_
	*active* while it runs, else *inactive*.

_/kernel/class/watchdog/watchdog0/timeout_, _min_timeout_, _max_timeout_
	Its timeout in seconds, as last set (0 before it first runs), and the
	shortest and longest it takes.

_/kernel/class/watchdog/watchdog0/bootstatus_
	*32*, Linux's WDIOF_CARDRESET, if it restarted the machine at the end
	of the boot before this one; *0* if not.

_/kernel/class/power_supply/_*name*_/type_, _online_
	For each AC adapter of the firmware's (_ACPI0003_, through uACPI;
	*dukovo-boot*(7)'s *ACPI*), by its name in the ACPI namespace, as
	*ADP1*: *Mains*, and *1* while it is plugged in, else *0*.

_/kernel/class/power_supply/_*name*_/type_, _present_, _status_
	For each battery of the firmware's (_PNP0C0A_), by its name, as
	*BAT0*, as Linux's ACPI battery shows it: *Battery*; *1* while it is
	in its bay, else *0*; and *Charging*, *Discharging*, *Full* or *Not
	charging*. Its state is read again when a file is read a second
	after the last read, or after the firmware notifies a change.

_/kernel/class/power_supply/_*name*_/capacity_, _capacity_level_
	What it has left, in percent of its last full charge (or, without
	one, of its design capacity), and *Critical*, *Low*, *Full* or
	*Normal*.

_/kernel/class/power_supply/_*name*_/energy_full_design_, _energy_full_, _energy_now_, _power_now_
	For a battery that counts in watts: its design capacity, its last
	full charge and what it has left, in µWh, and the rate it charges or
	discharges at, in µW. A battery that counts in amperes has
	_charge_full_design_, _charge_full_ and _charge_now_ (µAh) and
	_current_now_ (µA) instead.

_/kernel/class/power_supply/_*name*_/voltage_min_design_, _voltage_now_, _cycle_count_, _technology_
	Its design and present voltage, in µV; how many times it has been
	charged; and *Li-ion*, *Li-poly*, *NiMH*, *NiCd* or *Unknown*.

_/kernel/class/power_supply/_*name*_/model_name_, _manufacturer_, _serial_number_
	What the battery says it is.

	A value the battery does not know, and any but _type_ and _present_
	while its bay is empty, fails to read with EIO, as Linux's fail with
	ENODEV.

_/kernel/class/thermal/thermal_zone_*n*_/type_, _temp_
	For each thermal zone of the firmware's (*dukovo-boot*(7)'s *ACPI*),
	in the order the namespace has them, as Linux's ACPI thermal driver
	shows it: *acpitz*, and its temperature in thousandths of a degree
	Celsius, read from the zone's _\_TMP_ when the file is opened. It
	fails to read with EIO if _\_TMP_ fails. The sensors program of
	lm-sensors and the desktops' widgets read these.

_/kernel/class/thermal/thermal_zone_*n*_/mode_, _policy_, _available_policies_
	*enabled*, and *step_wise*, the only governor of the passive
	cooling.

_/kernel/class/thermal/thermal_zone_*n*_/trip_point_*k*_type_, _trip_point_*k*_temp_, _trip_point_*k*_hyst_
	Its trip points, in Linux's order: *critical*, *hot* and *passive*,
	each where the zone has one; its temperature, in thousandths of a
	degree Celsius, as the firmware last said it; and *0*.

_/kernel/class/thermal/cooling_device_*n*_/type_, _max_state_, _cur_state_
	Where the CPUs pick their own speed (*dukovo-boot*(7)'s *CPU
	FREQUENCY*), one for each: *Processor*; *3*; and how many steps,
	each a fifth of their highest level, passive cooling slows them by.

_/kernel/class/hwmon/hwmon_*n*_/name_
	The temperature sensors, as Linux's hwmon class shows them, numbered
	in this order: *acpitz*, the thermal zones, if the firmware has any;
	*k10temp*, the sensors of AMD's Zen processors; a *coretemp* for
	each package of Intel's processors (*dukovo-boot*(7)'s *SENSORS*);
	and *cros_ec*, the fans and sensors of Framework's embedded
	controller (*dukovo-boot*(7)'s *EMBEDDED CONTROLLER*). The sensors
	program of lm-sensors reads these.

_/kernel/class/hwmon/hwmon_*n*_/temp_*k*_input_, _temp_*k*_label_
	A sensor's temperature, in thousandths of a degree Celsius, read
	when the file is opened, and its name. For *acpitz*, the _k_-th
	zone's, without a label. For *k10temp*, *Tctl* (_temp1_), the
	temperature the processor's cooling acts on, and on models whose
	dies' registers Linux knows *Tccd1*, *Tccd2*... (from _temp3_), each
	core complex die's. For *coretemp*, *Package id* _p_ (_temp1_) and
	*Core* _c_ (_temp_*c+2*), read on a CPU of that package or core at
	most once a second. For *cros_ec*, each sensor the EC's memory map
	has, _temp1_ for its first place, with the name the EC gives it
	(*power-f75303*, *amd-apu*...). A sensor that gives no reading, as
	Intel's below the temperature they can measure, fails to read with
	EIO.

_/kernel/class/hwmon/hwmon_*n*_/temp_*k*_fault_, _fan_*k*_input_, _fan_*k*_fault_
	For *cros_ec*: *1* while the EC says the sensor failed, is not
	powered or not calibrated, else *0*; each fan's speed, in
	revolutions a minute, *0* when it is stopped or stalled; and *1*
	while an older EC says the fan stalled, else *0*.

_/kernel/class/hwmon/hwmon_*n*_/temp_*k*_crit_, _temp_*k*_max_, _temp_*k*_crit_alarm_
	For *acpitz*, a zone's critical trip point, where it has one. For
	*coretemp*: TjMax, where the processor's thermal control starts (100
	°C where it does not say); where it gives one, the temperature the
	control aims to keep below; and *1* once the critical temperature
	was reached, else *0*.

_/kernel/class/net/_*name*_/ifindex_, _iflink_
	The network interfaces, *lo* and the cards (*net0*, *net1*...),
	each a directory of its name, with its number as Linux numbers
	interfaces: *lo* is 1 and the cards follow from 2, in the order the
	kernel found them (ADR 0190). The C library's *if_nametoindex* and
	*if_nameindex* read these, and *ip*(8) the rest.

_/kernel/class/net/_*name*_/address_, _broadcast_, _addr_len_
	Its MAC address, as _52:54:00:12:34:56_, and the one that reaches
	every card on its network, all zeros for *lo*; and 6, their length.

_/kernel/class/net/_*name*_/mtu_, _type_, _flags_
	The longest packet it sends; *1* (ARPHRD_ETHER) for a card or *772*
	(ARPHRD_LOOPBACK) for *lo*; and Linux's flags, in hexadecimal:
	IFF_UP, with IFF_BROADCAST for a card or IFF_LOOPBACK for *lo*.

_/kernel/class/net/_*name*_/operstate_, _carrier_
	*up*, *down* for a card the kernel stopped after an error, or
	*unknown* for *lo*, which has no link; and *1* while it is not
	down, else *0*.

_/kernel/class/net/_*name*_/ipv4_
	Radix's own: its IPv4 addresses with their prefixes, a line each,
	as _10.0.2.15/24_, the first the one it sends from; nothing for a
	card not set up. *lo* shows 127.0.0.1/8, though it answers each
	card's addresses too. Linux gives these only over netlink.

_/kernel/class/net/_*name*_/statistics/rx_bytes_, _rx_packets_, _tx_bytes_, _tx_packets_, _tx_dropped_
	What it received and sent since boot, and what it could not send
	for want of room; _rx_errors_, _rx_dropped_ and _tx_errors_ are 0,
	as the stack counts none.

_/kernel/devices/system/cpu/possible_, _present_, _online_
	The CPUs the machine has, and those that run threads, as Linux's
	lists: *0-7*, which util-linux's lscpu reads.

_/kernel/devices/system/cpu/cpufreq/policy_*n*_/scaling_driver_
	Where CPU _n_ picks its own speed (*dukovo-boot*(7)'s *CPU
	FREQUENCY*): the Linux driver for its control, *intel_pstate* for
	Intel's HWP or *amd-pstate-epp* for AMD's CPPC. Each CPU has a
	policy, as with those drivers, and _/kernel/devices/system/cpu/cpu_*n*_/cpufreq_
	is a link to it. Linux's cpupower and powertop read these.

_/kernel/devices/system/cpu/cpufreq/policy_*n*_/affected_cpus_, _related_cpus_
	The CPU, *n*.

_/kernel/devices/system/cpu/cpufreq/policy_*n*_/cpuinfo_min_freq_, _cpuinfo_max_freq_
	Its lowest and highest frequency, in kHz.

_/kernel/devices/system/cpu/cpufreq/policy_*n*_/scaling_min_freq_, _scaling_max_freq_
	Those it is asked to stay between, in kHz: by default, all of them.

_/kernel/devices/system/cpu/cpufreq/policy_*n*_/scaling_cur_freq_
	The frequency it runs at, in kHz, measured over 10 ms when the file
	is read (or in the last 100 ms), from its APERF and MPERF counters;
	the base frequency if it slept throughout.

_/kernel/devices/system/cpu/cpufreq/policy_*n*_/scaling_governor_, _scaling_available_governors_
	*powersave*: the CPU picks its speed within the range itself, which
	Linux's drivers call so.

_/kernel/devices/system/cpu/cpufreq/policy_*n*_/energy_performance_preference_
	Where the CPU takes one, the energy preference it is asked for:
	*performance*, *balance_performance*, *balance_power* or *power*, or
	the number from 0 to 255 for another (*dukovo-boot*(7)'s *epp*).
	_energy_performance_available_preferences_ lists the names, and
	*default*, as Linux's.

_/kernel/devices/system/cpu/cpufreq/policy_*n*_/base_frequency_
	Intel's: the frequency it can keep up, in kHz.

_/kernel/devices/system/cpu/cpufreq/policy_*n*_/amd_pstate_highest_perf_, _amd_pstate_max_freq_, _amd_pstate_lowest_nonlinear_freq_
	AMD's: its highest level, its highest frequency, and its most
	efficient frequency (kHz).

	The files are read-only: the range and the preference are set at
	boot.

_/kernel/devices/system/edac/mc/mc0/ce_count_, _ue_count_
	While the CPUs' machine-check banks are read (*dukovo-boot*(7)'s
	*MACHINE CHECKS*): how many memory errors the memory controllers
	corrected, and how many they did not, since boot, as Linux's EDAC
	counts them. Radix shows them as one memory controller, whose DIMMs
	are the banks' channels.

_/kernel/devices/system/edac/mc/mc0/ce_noinfo_count_, _ue_noinfo_count_
	Those whose bank said no channel.

_/kernel/devices/system/edac/mc/mc0/mc_name_, _seconds_since_reset_
	*Machine check banks*, and the seconds since the counts began: since
	boot.

_/kernel/devices/system/edac/mc/mc0/dimm_*n*_/dimm_label_, _dimm_location_
	A bank and channel that had an error, such as _bank 7 channel 1_,
	numbered from 0 in the order they first did. Linux's
	*ras-mc-ctl --error-count* and *edac-util* read these.

_/kernel/devices/system/edac/mc/mc0/dimm_*n*_/dimm_ce_count_, _dimm_ue_count_
	Its corrected and uncorrected errors.

# EXAMPLES

In the *storage* boot test (*ob*(1)), the 32 MiB disk _vdb_ has
_/kernel/block/vdb/size_ of *65536*, and the boot test's program finds a
virtio function for each of its four disks in _/kernel/bus/pci/devices_.

# SEE ALSO

*radix-proc*(5), *radix-block*(2), *radix-files*(7), *dukovo-boot*(7), *ob*(1),
*spark*(8), *watchdog*(8), *lspci*(8), *lsmem*(1), *ip*(8)
