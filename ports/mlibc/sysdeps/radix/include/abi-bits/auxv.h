/* SPDX-FileCopyrightText: 2026 The Dukovo contributors */
/* SPDX-License-Identifier: Apache-2.0 OR MIT */

/* The entries of a program's auxiliary vector that getauxval reads:
 * Linux's, with Linux's numbers, as mlibc's abis/linux/auxv.h has them,
 * and Radix's own two (radix-process(2)'s AUX_CWD and AUX_STARTUP). */

#ifndef _ABIBITS_AUXV_H
#define _ABIBITS_AUXV_H

#define AT_PLATFORM 15
#define AT_HWCAP 16
#define AT_CLKTCK 17
#define AT_FPUCW 18
#define AT_SECURE 23
#define AT_RANDOM 25
#define AT_HWCAP2 26
#define AT_HWCAP3 29
#define AT_HWCAP4 30
#define AT_EXECFN 31
#define AT_SYSINFO_EHDR 33

/* The program's working directory, a string the C library keeps. */
#define AT_RADIX_CWD 0x52440001
/* The address of the program's start-up page (radix-startup(2)). */
#define AT_RADIX_STARTUP 0x52440002

#endif /* _ABIBITS_AUXV_H */
