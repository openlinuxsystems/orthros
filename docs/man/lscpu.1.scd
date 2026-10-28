lscpu(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

lscpu - what the processor is

# SYNOPSIS

*lscpu*

# DESCRIPTION

lscpu shows the machine's processor as util-linux's lscpu shows it on
Linux: its architecture and address sizes, how many CPUs the kernel runs
(_/kernel/cpus_, *radix-sysfs*(5)), its vendor, model name, family, model
and stepping, its threads per core, cores per socket and sockets, its
flags, whether it can run virtual machines or runs in one, and its
caches, each with its size summed over every instance.

```
$ lscpu
Architecture:             x86_64
  CPU op-mode(s):         32-bit, 64-bit
  Address sizes:          39 bits physical, 48 bits virtual
  Byte Order:             Little Endian
CPU(s):                   8
  On-line CPU(s) list:    0-7
Vendor ID:                GenuineIntel
  Model name:             Intel(R) Core(TM) i7-8550U CPU @ 1.80GHz
    CPU family:           6
    Model:                142
    Thread(s) per core:   2
    Core(s) per socket:   4
    Socket(s):            1
    Stepping:             10
    Flags:                fpu vme de pse tsc msr pae mce cx8 ...
Virtualization features:
  Virtualization:         VT-x
Caches (sum of all):
  L1d:                    128 KiB (4 instances)
  L1i:                    128 KiB (4 instances)
  L2:                     1 MiB (4 instances)
  L3:                     8 MiB (1 instance)
```

It reads the processor with the CPUID instruction, which a program may
run itself. The flags are the CPUID bits that Linux's _/proc/cpuinfo_
names, with Linux's names, without the flags that Linux's kernel adds
of its own, such as *constant_tsc*. The topology comes from CPUID too:
leaf 0xB where the processor has it. Linux's lscpu also shows clock
speeds, NUMA nodes and vulnerabilities, which this does not.

# EXIT STATUS

0, or 1 if _/kernel/cpus_ cannot be read or for an argument, which it
takes none of.

# SEE ALSO

*lspci*(8), *lsmem*(1), *radix-sysfs*(5), *systools*(1)
