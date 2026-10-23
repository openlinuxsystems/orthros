/*
 * SPDX-FileCopyrightText: 2026 The Dukovo contributors
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

/*
 * What the 4.4BSD db code needs that the C library does not give it, in
 * place of the compat.h the GNU C library wrote for it (ports/db1).
 */

#ifndef DB1_COMPAT_H
#define DB1_COMPAT_H

#include <errno.h>
#include <fcntl.h>
#include <sys/param.h>

/*
 * The 4.4BSD flags by which dbopen() locks the file as it opens it.
 * Without them it opens it unlocked, as on the GNU C library.
 */
#ifndef O_EXLOCK
#define O_EXLOCK 0
#endif
#ifndef O_SHLOCK
#define O_SHLOCK 0
#endif

/* 4.4BSD's error for a file that is not a database of the kind asked. */
#ifndef EFTYPE
#define EFTYPE EINVAL
#endif

#ifndef MAX
#define MAX(a, b) ((a) < (b) ? (b) : (a))
#endif
#ifndef MIN
#define MIN(a, b) ((a) < (b) ? (a) : (b))
#endif

#endif
