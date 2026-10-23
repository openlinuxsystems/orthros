/*
 * SPDX-FileCopyrightText: 2026 The Dukovo contributors
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

/* The functions of compat.h. */

#include <ctype.h>
#include <errno.h>
#include <string.h>

#include "compat.h"

const char *getprogname(void)
{
	return program_invocation_short_name;
}

int issetugid(void)
{
	return 0;
}

static const char digits[] =
	"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/*
 * Writes `src` in base64 to `target`, with a NUL after it. Returns the
 * length written without the NUL, or -1 if `targsize` cannot hold it.
 */
int b64_ntop(const unsigned char *src, size_t srclength, char *target, size_t targsize)
{
	size_t length = (srclength + 2) / 3 * 4;
	if (length >= targsize)
		return -1;
	char *out = target;
	for (; srclength >= 3; src += 3, srclength -= 3) {
		*out++ = digits[src[0] >> 2];
		*out++ = digits[(src[0] & 0x03) << 4 | src[1] >> 4];
		*out++ = digits[(src[1] & 0x0f) << 2 | src[2] >> 6];
		*out++ = digits[src[2] & 0x3f];
	}
	if (srclength > 0) {
		unsigned char second = srclength > 1 ? src[1] : 0;
		*out++ = digits[src[0] >> 2];
		*out++ = digits[(src[0] & 0x03) << 4 | second >> 4];
		*out++ = srclength > 1 ? digits[(second & 0x0f) << 2] : '=';
		*out++ = '=';
	}
	*out = '\0';
	return (int)length;
}

/*
 * Reads the base64 in `src` into `target`, skipping white space, and
 * returns how many bytes it wrote; or -1 if `src` is not base64 or
 * `targsize` cannot hold them. With `target` NULL, only counts them.
 */
int b64_pton(const char *src, unsigned char *target, size_t targsize)
{
	size_t written = 0;
	unsigned int bits = 0;
	int held = 0, padding = 0;
	for (; *src != '\0'; src++) {
		unsigned char ch = (unsigned char)*src;
		if (isspace(ch))
			continue;
		if (ch == '=') {
			padding++;
			continue;
		}
		const char *at = strchr(digits, ch);
		if (at == NULL || padding > 0)
			return -1;
		bits = bits << 6 | (unsigned int)(at - digits);
		held += 6;
		if (held >= 8) {
			held -= 8;
			if (target != NULL) {
				if (written >= targsize)
					return -1;
				target[written] = (unsigned char)(bits >> held);
			}
			written++;
		}
	}
	/*
	 * What is left over must be the zero bits that pad the last byte, and
	 * the '=' that end the text as many as the bytes of its last group
	 * call for.
	 */
	if ((bits & ((1u << held) - 1)) != 0)
		return -1;
	if (padding > 0 && padding != held / 2)
		return -1;
	return (int)written;
}
