/*
 * SPDX-FileCopyrightText: 2026 The Dukovo contributors
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

/*
 * The part of the BSD <bitstring.h> nvi uses: arrays of bits, which it
 * marks the characters that start a mapped key sequence in.
 */

#ifndef NVI_BITSTRING_H
#define NVI_BITSTRING_H

typedef unsigned char bitstr_t;

/* The bytes that hold `nbits` bits. */
#define bitstr_size(nbits) (((nbits) + 7) >> 3)

/* Declares `name` as an array of `nbits` bits: `bitstr_t bit_decl(...)`. */
#define bit_decl(name, nbits) ((name)[bitstr_size(nbits)])

#define bit_set(name, bit) ((name)[(bit) >> 3] |= (bitstr_t)(1 << ((bit) & 7)))
#define bit_clear(name, bit) ((name)[(bit) >> 3] &= (bitstr_t)~(1 << ((bit) & 7)))
#define bit_test(name, bit) ((name)[(bit) >> 3] & (1 << ((bit) & 7)))

#endif
