/*
 * SPDX-FileCopyrightText: 2026 The Dukovo contributors
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

/*
 * What nvi takes from the BSD C library that Radix's does not have,
 * included before each of its files (ports/nvi). compat.c has the
 * functions.
 */

#ifndef NVI_COMPAT_H
#define NVI_COMPAT_H

#include <stddef.h>
#include <sys/queue.h>

/* The list macros of FreeBSD's <sys/queue.h> that the C library's lacks. */
#ifndef SLIST_FOREACH_SAFE
#define SLIST_FOREACH_SAFE(var, head, field, next) \
	for ((var) = SLIST_FIRST(head); (var) != NULL && ((next) = SLIST_NEXT(var, field), 1); \
	     (var) = (next))
#endif
#ifndef SLIST_REMOVE_AFTER
#define SLIST_REMOVE_AFTER(elm, field) \
	(SLIST_NEXT(elm, field) = SLIST_NEXT(SLIST_NEXT(elm, field), field))
#endif
#ifndef TAILQ_FOREACH_SAFE
#define TAILQ_FOREACH_SAFE(var, head, field, next) \
	for ((var) = TAILQ_FIRST(head); (var) != NULL && ((next) = TAILQ_NEXT(var, field), 1); \
	     (var) = (next))
#endif

/* The name the program was run by: nvi is ex when run as ex. */
const char *getprogname(void);

/*
 * Whether the program runs with the rights of its file's owner, when it
 * must not trust its environment: never on Radix, where a program's
 * file gives it no rights (radix-posix(7)).
 */
int issetugid(void);

/*
 * Base64, which nvi writes the names in a recovery file's header in, as
 * BIND's resolver library has it.
 */
int b64_ntop(const unsigned char *src, size_t srclength, char *target, size_t targsize);
int b64_pton(const char *src, unsigned char *target, size_t targsize);

#endif
