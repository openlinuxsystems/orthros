hexdump(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

hexdump, hd - show a file's bytes in hex, octal, decimal or text

# SYNOPSIS

*hexdump* [*-bcCdovx*] [*-e* _format_] [*-f* _file_] [*-n* _length_] [*-s* _offset_] [_file_ ...]

*hd* [_option_ ...] [_file_ ...]

# DESCRIPTION

hexdump shows the bytes of each _file_ in turn, as one stream, or of
standard input, as util-linux's hexdump does: in the formats its
options name, or in formats given as format strings. Without either it
shows, a line each sixteen bytes, the offset and eight two-byte words
in hex. *hd* is *hexdump -C*:

	00000000  68 65 6c 6c 6f 2c 20 77  6f 72 6c 64 0a           |hello, world.|
	0000000d

Each format reads the same block of bytes, as many as the longest one
reads, and they print one after another. A block the same as the one
before is shown as a single *\**, unless *-v* is given. The offset after
the last byte ends the output.

It was written for Dukovo and has none of util-linux's or BSD's code.

# OPTIONS

*-C*, *--canonical*
	Offset, sixteen bytes in hex, and the bytes as text between bars
	(a dot for each that is not printable).

*-x*, *--two-bytes-hex*
	Two-byte words in hex.

*-d*, *--two-bytes-decimal*
	Two-byte words in unsigned decimal.

*-o*, *--two-bytes-octal*
	Two-byte words in octal.

*-b*, *--one-byte-octal*
	Bytes in octal.

*-c*, *--one-byte-char*
	Bytes as characters, C escapes or octal.

*-e*, *--format* _format_
	A format string, below.

*-f*, *--format-file* _file_
	Format strings, a line each; blank lines and those that start with
	*#* are left out.

*-n*, *--length* _length_
	Only _length_ bytes.

*-s*, *--skip* _offset_
	Start at byte _offset_.

*-v*, *--no-squeezing*
	Show every block, also those like the one before.

*-L*, *--color* are taken and change nothing: hexdump does not colour.

The words are in the machine's order, little-endian. _length_ and
_offset_ may be decimal, hexadecimal with *0x* or octal with *0*, and
end in *b* (512), in *K*, *M*, *G*, *T*, *P* or *E*, alone or with *iB*
(powers of 1024), or in one of them with *B* (powers of 1000): *4K*,
*4KiB* and *4096* are the same.

# FORMAT STRINGS

A format string is units, each

	\[_count_*/*\]\[_bytes_\] *"*_format_*"*

which applies _format_, a *printf*(3) format, _count_ times (once by
default), each time to the next _bytes_ bytes of the block. Without
_bytes_ each conversion takes its own: 4 for *%d*, *%i*, *%o*, *%u*,
*%x*, *%X*, 8 for *%e*, *%f*, *%g* and their capitals, 1 for *%c* and
hexdump's own; *%s* takes its precision's or all of _bytes_. Integer
conversions read 1, 2, 4 or 8 bytes, and floating ones 4 or 8. Escapes
*\\n*, *\\t*, *\\\\* and the others of C work in _format_.

hexdump's own conversions:

*%\_ad*, *%\_ao*, *%\_ax*
	The offset of the next byte, in decimal, octal or hex.

*%\_Ad*, *%\_Ao*, *%\_Ax*
	The offset after the last byte, printed once, at the end.

*%\_c*
	A byte as a character, a C escape (*\\n*), or three octal digits.

*%\_p*
	A byte as a printable character, or a dot.

*%\_u*
	A byte as a printable character, a control character's name
	(*nul*, *lf*, *del*), or hex.

The last unit without a count is repeated to fill the block, and a
blank that ends its text is left out after its last repetition. Where
the data ends within a block, each conversion prints blanks as wide as
it would have printed.

# EXIT STATUS

0, or 1 if a file could not be read or a format is wrong.

# EXAMPLES

The first 64 bytes of a disk:

	hd -n 64 /devices/vda

Bytes in decimal, eight a line, with the offset:

	hexdump -e '"%07.7\_ad  " 8/1 "%4u" "\\n"' file

# SEE ALSO

*xxd*(1), *systools*(1), *printf*(3)
