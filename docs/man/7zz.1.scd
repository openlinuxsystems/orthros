7zz(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

7zz, 7z - the 7-Zip archiver

# SYNOPSIS

*7zz* _command_ [_switch_ ...] _archive_ [_file_ ...] [@_listfile_ ...]

# DESCRIPTION

7zz is 7-Zip's standalone program, by Igor Pavlov: it makes, lists,
tests, changes and unpacks archives. It writes 7z, zip, tar, wim, xz,
bzip2 and gzip archives, and reads those and many more, among them
zstd, lzma, cpio, cab, arj, rpm and deb, ISO 9660 and UDF images, dmg,
VHD, VMDK, QCOW and other disk images, MBR and GPT partition tables,
and FAT, NTFS, ext, squashfs and other filesystems. It does not read RAR archives, whose
code is not free: *tar*(1) reads them. *7z* is another name for it.

The 7z format compresses with LZMA2 by default, splits an archive into
volumes, encrypts with AES-256, and can hide the names of what it holds
(*-mhe*). Files that would go in an archive are named after it, with
wildcards, which 7zz matches itself; directories are taken with
everything in them.

# COMMANDS

The command is the first argument, one letter or two.

*a*
	Add the files to the archive, making it if it does not exist.

*u*
	Update the archive: add the files that are newer than its copies of
	them, or that it lacks.

*d*
	Delete the files from the archive.

*rn* _old_ _new_ [_old_ _new_ ...]
	Rename files in the archive.

*l*
	List what the archive holds.

*t*
	Test the archive: unpack everything into nothing, checking every
	checksum.

*x*
	Extract the files, with the directories they are in.

*e*
	Extract the files, each into the output directory itself, without
	the directories they are in.

*h*
	Show the hash of each file (CRC32 unless *-scrc* names another).

*i*
	Show the formats, codecs and hashes it knows.

*b*
	Benchmark compression and decompression on this machine.

# SWITCHES

Switches go anywhere after the command, until a lone *--*. A switch's
argument follows it with no space between them; the braces, as in
7-Zip's own help, show where it goes and are not typed.

*-o*{_directory_}
	Extract into _directory_, made if it does not exist.

*-p*[_password_]
	The password to encrypt or decrypt with; with none, 7zz asks for it
	on the terminal, without showing it as it is typed.

*-t*{_type_}
	The archive's type: *7z*, *zip*, *tar*, *xz*, *gzip*, *bzip2*,
	*wim* and the rest that *i* lists. A new archive's type
	otherwise follows its name's ending, and is 7z for any other.

*-mx*{_N_}
	The level of compression, from *-mx0* (none: only stored) and
	*-mx1* (fastest) to *-mx9* (ultra); 5 by default.

*-mmt*[_N_]
	Compress with _N_ threads, or with *-mmt* alone, as many as there
	are processors.

*-mhe*
	In a 7z archive with a password, encrypt the names of the files as
	well.

*-m*{_parameter_}
	Other settings of the method, such as *-m0=lzma2* or *-md=64m* for
	the dictionary's size.

*-r*
	Look for files whose names match in each subdirectory too.

*-x*!_wildcard_, *-x*@_listfile_
	Leave out the files the wildcard, or each line of the list file,
	matches. *-i* takes in only those that match, the same way.

*-v*{_size_}[*b*|*k*|*m*|*g*]
	Make volumes of _size_ bytes, kibibytes, mebibytes or gibibytes.

*-ao*{_mode_}
	What extracting does when a file is already there: overwrite it
	(*a*), skip it (*s*), rename the new one (*u*) or the old one (*t*).

*-y*
	Answer yes to every question.

*-so*, *-si*[_name_]
	Write what is extracted to standard output, or read what is to be
	compressed, as a file called _name_, from standard input.

*-snl*
	Keep symbolic links as links, not the files they point to.

*-sdel*
	Delete the files once they are in the archive.

*-bb*[_N_]
	How much to say as it works, from 0 to 3.

*-bd*
	Show no progress.

*-slt*
	With *l*, show each file's every property, one a line.

*-w*[_directory_]
	Make the new archive in _directory_ first, by default the system's
	temporary one, and move it into place at the end.

# EXIT STATUS

*0*
	Everything worked.

*1*
	A warning: some files were not processed, such as one in use.

*2*
	A fatal error.

*7*
	The command line was wrong.

*8*
	There was not enough memory.

*255*
	It was stopped, as by Ctrl+C.

# EXAMPLES

Put a directory, with everything in it, in a new 7z archive:

	7zz a backup.7z /shared/projects

The same, at the best compression, with a password asked for and the
names hidden:

	7zz a -mx9 -p -mhe secret.7z notes

List an archive, test it, and extract it into _out_:

	7zz l backup.7z++
7zz t backup.7z++
7zz x backup.7z -oout

Make a zip archive, or a tar archive compressed with xz:

	7zz a -tzip photos.zip \*.jpg++
7zz a -ttar -so -an logs | 7zz a -txz -si logs.tar.xz

Show the SHA-256 of each file:

	7zz h -scrcSHA256 \*.iso

# SEE ALSO

*tar*(1), *unzip*(1), *zip*(1), *xz*(1), *zstd*(1)

7-Zip's documentation: https://www.7-zip.org

# COPYRIGHT

7-Zip is by Igor Pavlov, under the GNU LGPL 2.1 or later, with the
LZFSE and Zstandard decoders under the BSD 3-clause license and xxHash
under the BSD 2-clause license. This page is Dukovo's.
