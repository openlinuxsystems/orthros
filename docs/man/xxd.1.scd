xxd(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

xxd - make a hex dump, or turn one back into bytes

# SYNOPSIS

*xxd* [*-abCdeipu*] [*-c* _columns_] [*-g* _bytes_] [*-l* _length_] [*-n* _name_] [*-o* _offset_] [*-s* [*+*][*-*]_seek_] [_input_ [_output_]]

*xxd* *-r* [*-p*] [*-s* _offset_] [*-c* _columns_] [_input_ [_output_]]

# DESCRIPTION

xxd writes a hex dump of _input_ (standard input if it is missing or
*-*) to _output_ (standard output), with the options and layouts of
Vim's xxd: each line the offset of its first byte, the bytes in hex in
groups, and the bytes as text, a dot for each that is not printable.

	00000000: 6865 6c6c 6f2c 2077 6f72 6c64 0a         hello, world.

With *-r* it reads such a dump, or plain hex with *-p*, and writes the
bytes back. Each line goes where its offset says: an _output_ file is
patched, not cut short, so a line can change a few bytes of it, and a
gap is left as zeros. Standard output goes forwards only.

It was written for Dukovo: it has none of Vim's code.

# OPTIONS

*-c*, *--cols* _columns_
	Bytes a line: 16 by default, 30 with *-p*, 12 with *-i* and 6
	with *-b*; at most 256.

*-g*, *--groupsize* _bytes_
	Bytes a group: 2 by default, 4 with *-e*, 1 with *-b*; 0 for one
	group.

*-p*, *--ps*, *--plain*
	Plain hex, without offsets, groups or text.

*-b*, *--bits*
	Bits, not hex.

*-e*
	Each group as a little-endian number.

*-i*, *--include*
	A C array of the bytes and a variable with its length, named after
	_input_ or *-n*'s _name_.

*-n*, *--name* _name_
	The name of *-i*'s array.

*-C*, *--capitalize*
	*-i*'s names in capitals.

*-l*, *--len* _length_
	Stop after _length_ bytes.

*-s*, *--seek* [*+*][*-*]_seek_
	Start at byte _seek_ of _input_; *-*_seek_ that many bytes before
	its end, which needs a file. With *-r*, add _seek_ to each offset.

*-o* _offset_
	Add _offset_ to the offsets shown.

*-d*
	Offsets in decimal.

*-u*
	Hex in capitals.

*-a*, *--autoskip*
	A single *\** for a run of lines of zeros.

*-r*, *--revert*
	Turn a dump back into bytes.

*-R* _when_
	Taken and changes nothing: xxd does not colour.

*-v*, *--version*; *-h*, *--help*
	Its version; a usage line.

Numbers may be decimal, or hexadecimal with *0x*, or octal with *0*.
Vim's *-E* (EBCDIC) is not taken.

# EXIT STATUS

As Vim's xxd: 0; 1 for a mistake in the command line; 2 if _input_
cannot be opened; 3 if _output_ cannot be; 4 if _input_ cannot be sought
in; 5 if *-r* must go back on standard output.

# EXAMPLES

Change two bytes of a file:

	echo "0000006: 5757" | xxd -r - file

Embed a file in a C program:

	xxd -i logo.png > logo.h

# SEE ALSO

*hexdump*(1), *systools*(1)
