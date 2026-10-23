// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The ported utilities as a script uses them: each run through the links
// in /bin, with text on its standard input, and what it writes and its
// exit status checked. uutils' core utilities (ports/uutils-coreutils),
// find and xargs (ports/uutils-findutils), GNU grep, sed, diffutils and
// patch, awk, bc, less, ncurses' tools, man, the compressors, tar, cpio,
// zip and unzip, 7-Zip, file and jq (ports/<name>), and their manual
// pages; the editors, nano and vi, typed at on a pseudo-terminal; and
// tmux, its server and a client attached at a pseudo-terminal.

#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <termios.h>

#include "check.h"
#include "programs.h"

// Text: the filters of a pipeline.
static void text(void) {
	OUTPUT("", "hello world\n", "/bin/echo", "hello", "world");
	OUTPUT("abc\n", "abc\n", "/bin/cat");
	OUTPUT("b\na\nc\n", "a\nb\nc\n", "/bin/sort");
	OUTPUT("10\n9\n100\n", "9\n10\n100\n", "/bin/sort", "-n");
	OUTPUT("a\na\nb\n", "a\nb\n", "/bin/uniq");
	OUTPUT("1\n2\n3\n", "3\n", "/bin/wc", "-l");
	OUTPUT("dukovo\n", "DUKOVO\n", "/bin/tr", "a-z", "A-Z");
	OUTPUT("1\n2\n3\n", "1\n2\n", "/bin/head", "-n", "2");
	OUTPUT("1\n2\n3\n", "3\n", "/bin/tail", "-n", "1");
	OUTPUT("a:b:c\n", "b\n", "/bin/cut", "-d:", "-f2");
	OUTPUT("1\n2\n", "2\n1\n", "/bin/tac");
	OUTPUT("a\tb\n", "a       b\n", "/bin/expand");
	OUTPUT("", "1\n2\n3\n", "/bin/seq", "3");
	OUTPUT("", "00042", "/bin/printf", "%05d", "42");
	OUTPUT("AB", " 41 42\n", "/bin/od", "-An", "-tx1");
	OUTPUT("hi", "aGk=\n", "/bin/base64");
	OUTPUT("aGk=\n", "hi", "/bin/base64", "-d");
	OUTPUT("abc", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  -\n",
			"/bin/sha256sum");
	OUTPUT("abc", "900150983cd24fb0d6963f7d28e17f72  -\n", "/bin/md5sum");
	OUTPUT("", "5\n", "/bin/expr", "2", "+", "3");
	OUTPUT("", "12: 2 2 3\n", "/bin/factor", "12");
	OUTPUT("", "c\n", "/bin/basename", "/a/b/c.txt", ".txt");
	OUTPUT("", "/a/b\n", "/bin/dirname", "/a/b/c.txt");
	OUTPUT("", "1970\n", "/bin/date", "-u", "-d", "@0", "+%Y");
	OUTPUT("", "Dukovo\n", "/bin/uname", "-s");
}

// Exit statuses: test and its other name, true and false, timeout.
static void statuses(void) {
	STATUS(0, "/bin/true");
	STATUS(1, "/bin/false");
	STATUS(0, "/bin/test", "-d", "/tmp");
	STATUS(0, "/bin/test", "-L", "/bin");
	STATUS(1, "/bin/test", "-f", "/nowhere");
	STATUS(0, "/bin/[", "1", "-lt", "2", "]");
	STATUS(1, "/bin/[", "b", "=", "a", "]");
	STATUS(0, "/bin/sleep", "0.05");
	STATUS(124, "/bin/timeout", "0.2", "/bin/sleep", "5");
	OUTPUT("", "bar\n", "/bin/env", "FOO=bar", "/bin/printenv", "FOO");
}

// Files: making, linking, copying, moving, listing and removing them.
static void files(void) {
	STATUS(0, "/bin/mkdir", "-p", "/tmp/utilities/a/b");
	STATUS(0, "/bin/touch", "/tmp/utilities/a/b/file");
	OUTPUT("text\n", "text\n", "/bin/tee", "/tmp/utilities/a/text");
	OUTPUT("", "text\n", "/bin/cat", "/tmp/utilities/a/text");
	STATUS(0, "/bin/cp", "/tmp/utilities/a/text", "/tmp/utilities/copy");
	STATUS(0, "/bin/mv", "/tmp/utilities/copy", "/tmp/utilities/moved");
	STATUS(0, "/bin/ln", "-s", "a/b", "/tmp/utilities/link");
	OUTPUT("", "a/b\n", "/bin/readlink", "/tmp/utilities/link");
	OUTPUT("", "/tmp/utilities/a/b\n", "/bin/realpath", "/tmp/utilities/link");
	OUTPUT("", "symbolic link\n", "/bin/stat", "-c", "%F", "/tmp/utilities/link");
	OUTPUT("", "5\n", "/bin/stat", "-c", "%s", "/tmp/utilities/moved");
	OUTPUT("", "a\nlink\nmoved\n", "/bin/ls", "/tmp/utilities");
	CONTAINS("-> a/b", "/bin/ls", "-l", "/tmp/utilities");
	STATUS(0, "/bin/cp", "-r", "/tmp/utilities/a", "/tmp/utilities/tree");
	OUTPUT("", "/tmp/utilities/tree/b/file\n", "/bin/ls", "-d", "/tmp/utilities/tree/b/file");
	CONTAINS("/tmp/utilities\n", "/bin/du", "-s", "/tmp/utilities");
	STATUS(0, "/bin/rm", "-r", "/tmp/utilities");
	STATUS(2, "/bin/ls", "/tmp/utilities");
	// Spark's links into /system, and what ls and realpath make of them.
	OUTPUT("", "system\n", "/bin/readlink", "/usr");
	OUTPUT("", "/system/bin\n", "/bin/realpath", "/usr/bin");
	CONTAINS("-> system/bin", "/bin/ls", "-ld", "/bin");
	CONTAINS("coreutils\n", "/bin/ls", "/system/bin");
}

// find and xargs.
static void finding(void) {
	STATUS(0, "/bin/mkdir", "-p", "/tmp/find/a/b");
	STATUS(0, "/bin/touch", "/tmp/find/a/one.txt", "/tmp/find/a/b/two.txt", "/tmp/find/c");
	STATUS(0, "/bin/ln", "-s", "a", "/tmp/find/l");
	OUTPUT("", "/tmp/find/a/b/two.txt\n/tmp/find/a/one.txt\n", "/bin/find", "/tmp/find",
			"-sorted", "-name", "*.txt");
	OUTPUT("", "/tmp/find/l\n", "/bin/find", "/tmp/find", "-type", "l");
	OUTPUT("", "/tmp/find/a\n/tmp/find/c\n/tmp/find/l\n", "/bin/find", "/tmp/find", "-sorted",
			"-mindepth", "1", "-maxdepth", "1");
	OUTPUT("", "/tmp/find/l/b\n", "/bin/find", "-L", "/tmp/find/l", "-type", "d", "-name", "b");
	OUTPUT("", "c\n", "/bin/find", "/tmp/find", "-name", "c", "-printf", "%f\\n");
	OUTPUT("", "/tmp/find/c\n", "/bin/find", "/tmp/find", "-name", "c", "-exec", "/bin/echo",
			"{}", ";");
	OUTPUT("", "/proc\n", "/bin/find", "/proc", "-maxdepth", "0", "-fstype", "proc");
	OUTPUT("", "/tmp\n", "/bin/find", "/tmp", "-maxdepth", "0", "-fstype", "tmpfs");
	STATUS(0, "/bin/find", "/tmp/find", "-name", "*.txt", "-delete");
	OUTPUT("", "", "/bin/find", "/tmp/find", "-name", "*.txt");
	STATUS(1, "/bin/find", "/nowhere");
	OUTPUT("1 2 3", "1\n2\n3\n", "/bin/xargs", "-n", "1", "/bin/echo");
	OUTPUT("a b\0c\0", "a b c\n", "/bin/xargs", "-0", "/bin/echo");
	OUTPUT("x\ny\n", "<x>\n<y>\n", "/bin/xargs", "-I", "{}", "/bin/echo", "<{}>");
	OUTPUT("", "", "/bin/xargs", "-r", "/bin/echo", "never");
	STATUS(0, "/bin/rm", "-r", "/tmp/find");
}

// grep, sed and awk.
static void searching(void) {
	OUTPUT("a\nb\nab\n", "a\nab\n", "/bin/grep", "a");
	OUTPUT("A\nb\n", "A\n", "/bin/grep", "-i", "a");
	OUTPUT("a\nb\nab\n", "b\n", "/bin/grep", "-v", "a");
	OUTPUT("a\nb\nab\n", "2\n", "/bin/grep", "-c", "a");
	OUTPUT("x1\nfoo22\n", "2:foo22\n", "/bin/grep", "-n", "-E", "o{2}[0-9]{2}$");
	OUTPUT("a.b\naxb\n", "a.b\n", "/bin/grep", "-F", "a.b");
	OUTPUT("cat category\n", "cat\n", "/bin/grep", "-o", "-w", "cat");
	STATUS(1, "/bin/grep", "x");
	STATUS(2, "/bin/grep", "x", "/nowhere");
	// egrep is a script, which /bin/sh runs.
	OUTPUT("a\nb\nc\n", "a\nc\n", "/bin/egrep", "a|c");
	STATUS(0, "/bin/mkdir", "-p", "/tmp/search/d");
	OUTPUT("needle\n", "needle\n", "/bin/tee", "/tmp/search/d/file");
	OUTPUT("", "/tmp/search/d/file:needle\n", "/bin/grep", "-r", "needle", "/tmp/search");

	OUTPUT("hello\n", "hallo\n", "/bin/sed", "s/e/a/");
	OUTPUT("1\n2\n3\n", "2\n", "/bin/sed", "-n", "2p");
	OUTPUT("a b\n", "b a\n", "/bin/sed", "-E", "s/(a) (b)/\\2 \\1/");
	OUTPUT("a\nb\nc\n", "a\nc\n", "/bin/sed", "/b/d");
	OUTPUT("x\n", "x\ny\n", "/bin/sed", "a\\\ny");
	STATUS(0, "/bin/sed", "-i", "s/needle/thread/", "/tmp/search/d/file");
	OUTPUT("", "thread\n", "/bin/cat", "/tmp/search/d/file");

	OUTPUT("a 1\nb 2\n", "3\n", "/bin/awk", "{ s += $2 } END { print s }");
	OUTPUT("a:b\n", "b\n", "/bin/awk", "-F:", "{ print $2 }");
	OUTPUT("", "3.14\n", "/bin/awk", "BEGIN { printf \"%.2f\\n\", 3.14159 }");
	OUTPUT("dukovo os\n", "DUKOVO 6\n", "/bin/awk", "{ print toupper($1), length($1) }");
	OUTPUT("x=1\ny=2\n", "y\n", "/bin/awk", "-F=", "$2 > 1 { print $1 }");
	OUTPUT("", "3\n", "/bin/awk", "BEGIN { n = split(\"a,b,c\", parts, \",\"); print n }");
	STATUS(2, "/bin/awk", "BEGIN { exit 2 }");
	STATUS(0, "/bin/rm", "-r", "/tmp/search");
}

// diff, cmp, diff3, sdiff and patch, on files and trees.
static void comparing(void) {
	STATUS(0, "/bin/mkdir", "-p", "/tmp/compare/a", "/tmp/compare/b");
	OUTPUT("one\ntwo\nthree\n", "one\ntwo\nthree\n", "/bin/tee", "/tmp/compare/a/f");
	OUTPUT("one\n2\nthree\n", "one\n2\nthree\n", "/bin/tee", "/tmp/compare/b/f");
	OUTPUT("one\n  two\nthree\n", "one\n  two\nthree\n", "/bin/tee", "/tmp/compare/spaced");
	STATUS(0, "/bin/diff", "/tmp/compare/a/f", "/tmp/compare/a/f");
	RUN_WITH(1, "", "2c2\n< two\n---\n> 2\n", 0, "/bin/diff", "/tmp/compare/a/f",
			"/tmp/compare/b/f");
	RUN_WITH(1, "", "@@ -1,3 +1,3 @@\n one\n-two\n+2\n three\n", 1, "/bin/diff", "-u",
			"/tmp/compare/a/f", "/tmp/compare/b/f");
	STATUS(0, "/bin/diff", "-w", "/tmp/compare/a/f", "/tmp/compare/spaced");
	RUN_WITH(1, "", "diff -r /tmp/compare/a/f /tmp/compare/b/f\n", 1, "/bin/diff", "-r",
			"/tmp/compare/a", "/tmp/compare/b");
	STATUS(0, "/bin/cmp", "-s", "/tmp/compare/a/f", "/tmp/compare/a/f");
	STATUS(1, "/bin/cmp", "-s", "/tmp/compare/a/f", "/tmp/compare/b/f");
	STATUS(0, "/bin/diff3", "/tmp/compare/a/f", "/tmp/compare/a/f", "/tmp/compare/a/f");
	STATUS(1, "/bin/sdiff", "-s", "/tmp/compare/a/f", "/tmp/compare/b/f");

	// A patch made by diff, applied, and taken back.
	STATUS(1, "/bin/sh", "-c",
			"diff -u /tmp/compare/a/f /tmp/compare/b/f >/tmp/compare/change.diff");
	STATUS(0, "/bin/patch", "-s", "/tmp/compare/a/f", "/tmp/compare/change.diff");
	STATUS(0, "/bin/cmp", "/tmp/compare/a/f", "/tmp/compare/b/f");
	STATUS(0, "/bin/patch", "-s", "-R", "/tmp/compare/a/f", "/tmp/compare/change.diff");
	OUTPUT("", "one\ntwo\nthree\n", "/bin/cat", "/tmp/compare/a/f");
	// git's patches name files with a/ and b/, and rename them.
	OUTPUT("diff --git a/f b/g\nsimilarity index 100%\nrename from f\nrename to g\n", "",
			"/bin/patch", "-s", "-p1", "-d", "/tmp/compare/b");
	OUTPUT("", "g\n", "/bin/ls", "/tmp/compare/b");
	STATUS(0, "/bin/rm", "-r", "/tmp/compare");
}

// bc, dc, less, the terminal descriptions and man.
static void others(void) {
	OUTPUT("2^100\n", "1267650600228229401496703205376\n", "/bin/bc");
	OUTPUT("scale=10; sqrt(2)\n", "1.4142135623\n", "/bin/bc");
	OUTPUT("define f(n) { if (n < 2) return (n); return (f(n - 1) + f(n - 2)); }\nf(20)\n",
			"6765\n", "/bin/bc");
	OUTPUT("2 3 + p\n", "5\n", "/bin/dc");
	// less shows its input unchanged when its output is not a terminal.
	OUTPUT("line\n", "line\n", "/bin/less");
	OUTPUT("", "80\n", "/bin/tput", "-T", "xterm", "cols");
	// xterm's description also clears the lines scrolled off the screen.
	OUTPUT("", "\033[H\033[2J\033[3J", "/bin/tput", "-T", "xterm", "clear");
	STATUS(0, "/bin/tput", "-T", "tmux-256color", "colors");
	CONTAINS("cols#80,", "/bin/infocmp", "-1", "vt100");
	STATUS(3, "/bin/tput", "-T", "no-such-terminal", "cols");
	OUTPUT("", "/system/share/man/man1/grep.1\n", "/bin/man", "-w", "grep");
	// The pages are longer than what this collects, so a script reads
	// their start, where their NAME section is.
	CONTAINS("print lines that match patterns", "/bin/sh", "-c", "man grep | head -n 8");
	CONTAINS("stream editor", "/bin/sh", "-c", "man 1 sed | head -n 8");
	CONTAINS("terminal capability database", "/bin/sh", "-c", "man 5 terminfo | head -n 8");
}

// Each compressor, each way, through a pipe, and the scripts that read
// what they made.
static void compressing(void) {
	OUTPUT("text\n", "text\n", "/bin/sh", "-c", "gzip | gunzip");
	OUTPUT("text\n", "text\n", "/bin/sh", "-c", "gzip -9 | zcat");
	OUTPUT("text\n", "text\n", "/bin/sh", "-c", "bzip2 | bunzip2");
	OUTPUT("text\n", "text\n", "/bin/sh", "-c", "bzip2 | bzcat");
	OUTPUT("text\n", "text\n", "/bin/sh", "-c", "xz | unxz");
	OUTPUT("text\n", "text\n", "/bin/sh", "-c", "xz -T0 -e | xzcat");
	OUTPUT("text\n", "text\n", "/bin/sh", "-c", "lzma | unlzma");
	OUTPUT("text\n", "text\n", "/bin/sh", "-c", "zstd | unzstd");
	OUTPUT("text\n", "text\n", "/bin/sh", "-c", "zstd -T0 -12 | zstdcat");
	OUTPUT("text\n", "text\n", "/bin/sh", "-c", "lz4 | unlz4");
	OUTPUT("text\n", "text\n", "/bin/sh", "-c", "lz4 -9 | lz4cat");
	// zstd reads the others' formats with their libraries.
	OUTPUT("text\n", "text\n", "/bin/sh", "-c", "gzip | zstd -dc");
	OUTPUT("text\n", "text\n", "/bin/sh", "-c", "xz | zstd -dc");
	OUTPUT("text\n", "text\n", "/bin/sh", "-c", "lz4 | zstd -dc");
	// Damaged data is an error.
	STATUS(1, "/bin/sh", "-c", "echo text | gzip -t");
	STATUS(1, "/bin/sh", "-c", "echo text | xz -t");
	STATUS(1, "/bin/sh", "-c", "echo text | zstd -t");

	STATUS(0, "/bin/mkdir", "-p", "/tmp/compress");
	OUTPUT("one\nneedle\n", "one\nneedle\n", "/bin/tee", "/tmp/compress/f");
	STATUS(0, "/bin/sh", "-c",
			"cd /tmp/compress && gzip -k f && bzip2 -k f && xz -k f && zstd -q f && lz4 -q f");
	OUTPUT("", "f\nf.bz2\nf.gz\nf.lz4\nf.xz\nf.zst\n", "/bin/ls", "/tmp/compress");
	OUTPUT("", "needle\n", "/bin/zgrep", "needle", "/tmp/compress/f.gz");
	OUTPUT("", "needle\n", "/bin/bzgrep", "needle", "/tmp/compress/f.bz2");
	OUTPUT("", "needle\n", "/bin/xzgrep", "needle", "/tmp/compress/f.xz");
	OUTPUT("", "needle\n", "/bin/zstdgrep", "needle", "/tmp/compress/f.zst");
	STATUS(1, "/bin/zgrep", "thread", "/tmp/compress/f.gz");
	STATUS(0, "/bin/zcmp", "/tmp/compress/f.gz", "/tmp/compress/f");
	STATUS(0, "/bin/xzdiff", "/tmp/compress/f.xz", "/tmp/compress/f");
	OUTPUT("", "one\nneedle\n", "/bin/bsdcat", "/tmp/compress/f.zst");
	CONTAINS("% /tmp/compress/f\n", "/bin/gzip", "-l", "/tmp/compress/f.gz");
	STATUS(0, "/bin/gunzip", "-f", "/tmp/compress/f.gz");
	OUTPUT("", "f\nf.bz2\nf.lz4\nf.xz\nf.zst\n", "/bin/ls", "/tmp/compress");
	STATUS(0, "/bin/rm", "-r", "/tmp/compress");
}

// tar, cpio, zip and unzip, and 7-Zip: archives made, listed and
// unpacked, with each compressor, and read by the others' programs.
static void archiving(void) {
	STATUS(0, "/bin/mkdir", "-p", "/tmp/archive/d/e", "/tmp/archive/out");
	OUTPUT("first\n", "first\n", "/bin/tee", "/tmp/archive/d/a");
	OUTPUT("second\n", "second\n", "/bin/tee", "/tmp/archive/d/e/b");
	STATUS(0, "/bin/ln", "-s", "a", "/tmp/archive/d/l");
	const char *const tars[] = {"t.tar", "t.tar.gz", "t.tar.bz2", "t.tar.xz", "t.tar.zst",
			"t.tar.lz4"};
	for(size_t i = 0; i < sizeof(tars) / sizeof(tars[0]); i++) {
		char make[128], list[128];
		snprintf(make, sizeof(make), "cd /tmp/archive && tar -acf %s d", tars[i]);
		snprintf(list, sizeof(list), "tar -tf /tmp/archive/%s | sort", tars[i]);
		STATUS(0, "/bin/sh", "-c", make);
		OUTPUT("", "d/\nd/a\nd/e/\nd/e/b\nd/l\n", "/bin/sh", "-c", list);
	}
	STATUS(0, "/bin/tar", "-xzf", "/tmp/archive/t.tar.gz", "-C", "/tmp/archive/out");
	OUTPUT("", "second\n", "/bin/cat", "/tmp/archive/out/d/e/b");
	OUTPUT("", "a\n", "/bin/readlink", "/tmp/archive/out/d/l");
	OUTPUT("", "first\n", "/bin/tar", "-xOf", "/tmp/archive/t.tar.xz", "d/a");
	CONTAINS("d/e/b", "/bin/tar", "-tvf", "/tmp/archive/t.tar.zst");
	STATUS(1, "/bin/tar", "-xf", "/tmp/archive/d/a");

	// cpio's SVR4 format, out and back in.
	STATUS(0, "/bin/sh", "-c",
			"cd /tmp/archive && find d | cpio -o --quiet -H newc >t.cpio");
	OUTPUT("", "d\nd/a\nd/e\nd/e/b\nd/l\n", "/bin/sh", "-c",
			"cpio -it --quiet </tmp/archive/t.cpio | sort");
	STATUS(0, "/bin/sh", "-c",
			"mkdir /tmp/archive/c && cd /tmp/archive/c && cpio -id --quiet </tmp/archive/t.cpio");
	OUTPUT("", "first\n", "/bin/cat", "/tmp/archive/c/d/a");

	// zip makes archives; unzip and tar read them.
	STATUS(0, "/bin/sh", "-c", "cd /tmp/archive && zip -q -r -y t.zip d");
	OUTPUT("", "first\n", "/bin/unzip", "-p", "/tmp/archive/t.zip", "d/a");
	STATUS(0, "/bin/unzip", "-q", "-t", "/tmp/archive/t.zip");
	STATUS(0, "/bin/unzip", "-q", "-d", "/tmp/archive/z", "/tmp/archive/t.zip");
	OUTPUT("", "second\n", "/bin/cat", "/tmp/archive/z/d/e/b");
	OUTPUT("", "second\n", "/bin/tar", "-xOf", "/tmp/archive/t.zip", "d/e/b");
	CONTAINS("d/e/b", "/bin/unzip", "-l", "/tmp/archive/t.zip");
	CONTAINS("@ d/e/b\n", "/bin/zipnote", "/tmp/archive/t.zip");
	// zip -T checks for Info-ZIP's UnZip, so libarchive's is named.
	STATUS(0, "/bin/zip", "-q", "-T", "-TT", "unzip -q -t", "/tmp/archive/t.zip");
	STATUS(0, "/bin/zip", "-q", "-d", "/tmp/archive/t.zip", "d/l");
	STATUS(1, "/bin/sh", "-c", "unzip -l /tmp/archive/t.zip | grep -q d/l");
	// tar makes zip and 7z archives too.
	STATUS(0, "/bin/sh", "-c", "cd /tmp/archive && tar -acf t.7z d && tar --format zip -cf u.zip d");
	OUTPUT("", "first\n", "/bin/tar", "-xOf", "/tmp/archive/t.7z", "d/a");
	OUTPUT("", "first\n", "/bin/unzip", "-p", "/tmp/archive/u.zip", "d/a");
	// 7-Zip makes 7z archives, with a password and hidden names too, which
	// tar reads, and reads tar's.
	STATUS(0, "/bin/sh", "-c", "cd /tmp/archive && 7zz a -bso0 -bsp0 s.7z d");
	OUTPUT("", "first\n", "/bin/7zz", "e", "-so", "-bso0", "-bsp0", "/tmp/archive/s.7z", "d/a");
	OUTPUT("", "first\n", "/bin/tar", "-xOf", "/tmp/archive/s.7z", "d/a");
	OUTPUT("", "first\n", "/bin/7z", "e", "-so", "-bso0", "-bsp0", "/tmp/archive/t.7z", "d/a");
	STATUS(0, "/bin/sh", "-c",
			"cd /tmp/archive && 7zz x -bso0 -bsp0 -oseven s.7z && cmp seven/d/e/b d/e/b");
	STATUS(0, "/bin/sh", "-c", "cd /tmp/archive && 7zz a -bso0 -bsp0 -psecret -mhe p.7z d");
	OUTPUT("", "second\n", "/bin/7zz", "e", "-so", "-bso0", "-bsp0", "-psecret",
			"/tmp/archive/p.7z", "d/e/b");
	STATUS(2, "/bin/7zz", "t", "-bso0", "-bsp0", "-bse0", "-pwrong", "/tmp/archive/p.7z");
	STATUS(0, "/bin/rm", "-r", "/tmp/archive");
}

// What file makes of each kind of file, and jq.
static void describing(void) {
	STATUS(0, "/bin/mkdir", "-p", "/tmp/describe");
	OUTPUT("text\n", "text\n", "/bin/tee", "/tmp/describe/t");
	STATUS(0, "/bin/sh", "-c",
			"cd /tmp/describe && gzip -k t && bzip2 -k t && xz -k t && zstd -q t && lz4 -q t "
			"&& tar -cf t.tar t && zip -q t.zip t && echo t | cpio -o --quiet -H newc >t.cpio");
	const char *const kinds[][2] = {{"t", "text/plain"}, {"t.gz", "application/gzip"},
			{"t.bz2", "application/x-bzip2"}, {"t.xz", "application/x-xz"},
			{"t.zst", "application/zstd"}, {"t.lz4", "application/x-lz4"},
			{"t.tar", "application/x-tar"}, {"t.zip", "application/zip"},
			{"t.cpio", "application/x-cpio"}};
	for(size_t i = 0; i < sizeof(kinds) / sizeof(kinds[0]); i++) {
		char path[64], expected[64];
		snprintf(path, sizeof(path), "/tmp/describe/%s", kinds[i][0]);
		snprintf(expected, sizeof(expected), "%s\n", kinds[i][1]);
		expect(__FILE__, __LINE__, 0, "", 0, expected, 0,
				(const char *const[]){"/bin/file", "-b", "--mime-type", path, NULL});
	}
	OUTPUT("", "ASCII text\n", "/bin/file", "-b", "/tmp/describe/t");
	// Inside the compressed file.
	CONTAINS("ASCII text (XZ compressed data", "/bin/file", "-bz", "/tmp/describe/t.xz");
	CONTAINS("ELF 64-bit LSB pie executable, x86-64", "/bin/file", "-b", "/system/bin/jq");
	CONTAINS("directory", "/bin/file", "/tmp/describe");
	CONTAINS("symbolic link to system/bin", "/bin/file", "/bin");
	STATUS(0, "/bin/rm", "-r", "/tmp/describe");

	OUTPUT("{\"a\":1,\"b\":[1,2]}", "1\n", "/bin/jq", ".a");
	OUTPUT("{\"a\":1,\"b\":[1,2]}", "[2,4]\n", "/bin/jq", "-c", ".b | map(. * 2)");
	OUTPUT("{\"name\":\"Duke\"}", "Duke\n", "/bin/jq", "-r", ".name");
	OUTPUT("", "{\n  \"os\": \"Dukovo\"\n}\n", "/bin/jq", "-n", "--arg", "os", "Dukovo",
			"{os: $os}");
	OUTPUT("", "3\n", "/bin/jq", "-n", "[range(3)] | add");
	OUTPUT("[3,1,2]", "[1,2,3]\n", "/bin/jq", "-c", "sort");
	// Oniguruma's regular expressions.
	OUTPUT("\"abc-123\"", "true\n\"123\"\n", "/bin/jq",
			"test(\"^[a-z]+-\"), (capture(\"(?<n>[0-9]+)\") | .n)");
	// decNumber keeps numbers too large for a double exact.
	OUTPUT("100000000000000000000000000001\n", "100000000000000000000000000001\n", "/bin/jq", ".");
	// The C library's obsolete drem and significand.
	OUTPUT("", "1.25\n-1\n", "/bin/jq", "-n", "(10 | significand), drem(5; 3)");
	OUTPUT("", "1425599507\n", "/bin/jq", "-n", "\"2015-03-05T23:51:47Z\" | fromdate");
	OUTPUT("", "\"2015-03-05T23:51:47Z\"\n", "/bin/jq", "-n", "1425599507 | todate");
	RUN_WITH(1, "{}", "null\n", 0, "/bin/jq", "-e", ".missing");
	RUN_WITH(5, "{", "", 0, "/bin/jq", ".");
}

// Runs `argv` on a new pseudo-terminal of 24 lines of 80 columns, with
// TERM=xterm, as someone at a terminal would: waits until it has written
// `ready`, which it does once it has drawn its screen and so takes keys
// as they are typed, types `keys`, and reads what it writes until it
// ends, within 20 seconds. Returns its exit status, or -1 if it did not
// get that far; what it wrote is in `output`, as much as that holds.
static int on_terminal(const char *const argv[], const char *ready, const char *keys) {
	int master = posix_openpt(O_RDWR | O_NOCTTY);
	if(master < 0 || grantpt(master) < 0 || unlockpt(master) < 0)
		return -1;
	struct winsize size = {.ws_row = 24, .ws_col = 80};
	const char *name = ptsname(master);
	if(!name || ioctl(master, TIOCSWINSZ, &size) < 0)
		return -1;
	output[0] = 0;
	pid_t pid = fork();
	if(pid < 0)
		return -1;
	if(!pid) {
		// Opening the terminal in a new session makes it the controlling
		// terminal.
		setsid();
		int tty = open(name, O_RDWR);
		if(tty < 0)
			_exit(126);
		dup2(tty, STDIN_FILENO);
		dup2(tty, STDOUT_FILENO);
		dup2(tty, STDERR_FILENO);
		if(tty > STDERR_FILENO)
			close(tty);
		close(master);
		setenv("TERM", "xterm", 1);
		execv(argv[0], (char *const *)argv);
		_exit(127);
	}
	struct timespec now, end;
	clock_gettime(CLOCK_MONOTONIC, &end);
	end.tv_sec += 20;
	size_t len = 0;
	int typed = 0, status = -1;
	struct pollfd entry = {.fd = master, .events = POLLIN};
	do {
		char buffer[512];
		ssize_t got = poll(&entry, 1, 100) == 1 ? read(master, buffer, sizeof(buffer)) : 0;
		if(got > 0) {
			size_t take = sizeof(output) - 1 - len;
			take = (size_t)got < take ? (size_t)got : take;
			memcpy(output + len, buffer, take);
			len += take;
			output[len] = 0;
		} else if(got < 0) {
			// The terminal has closed: the program is ending.
			poll(NULL, 0, 10);
		}
		if(!typed && strstr(output, ready)) {
			if(write(master, keys, strlen(keys)) != (ssize_t)strlen(keys))
				break;
			typed = 1;
		}
		int got_status;
		if(waitpid(pid, &got_status, WNOHANG) == pid) {
			status = WIFEXITED(got_status) ? WEXITSTATUS(got_status) : 128 + WTERMSIG(got_status);
			pid = 0;
			break;
		}
		clock_gettime(CLOCK_MONOTONIC, &now);
	} while(now.tv_sec < end.tv_sec || (now.tv_sec == end.tv_sec && now.tv_nsec < end.tv_nsec));
	if(pid) {
		kill(pid, SIGKILL);
		waitpid(pid, NULL, 0);
	}
	close(master);
	return typed ? status : -1;
}

// Checks that `argv`, run on a terminal and typed `keys` at once it has
// written `ready`, exits with 0.
static void type_at(const char *file, int line, const char *ready, const char *keys,
		const char *const argv[]) {
	int got = on_terminal(argv, ready, keys);
	if(got != 0) {
		size_t len = strlen(output);
		char what[512];
		snprintf(what, sizeof(what), "%s exited with %d; it last wrote \"%s\"", argv[0], got,
				output + (len > 300 ? len - 300 : 0));
		check_failed(file, line, what, 0);
	}
}
#define TYPE_AT(ready, keys, ...) \
	type_at(__FILE__, __LINE__, ready, keys, (const char *const[]){__VA_ARGS__, NULL})

// nano and vi: each edits a file typed at on a terminal, nano with the
// key bindings the system's nanorc gives it, which Spark copied into
// /etc; and ex edits by a script.
static void editing(void) {
	STATUS(0, "/bin/mkdir", "-p", "/tmp/editing");
	CONTAINS("GNU nano, version 9.2", "/bin/nano", "--version");
	CONTAINS("bind ^S savefile main", "/bin/cat", "/etc/nanorc");
	// Typed text, Ctrl+Z to undo it, more text, Ctrl+S to save and Ctrl+Q
	// to quit.
	TYPE_AT("n.txt", "first\032second\023\021", "/bin/nano", "/tmp/editing/n.txt");
	OUTPUT("", "second\n", "/bin/cat", "/tmp/editing/n.txt");

	// vi: insert, Escape, then :wq. It keeps a copy of what it edits to
	// recover from in /var/tmp/vi.recover, which it makes.
	TYPE_AT("v.txt", "ihello from vi\033:wq\r", "/bin/vi", "/tmp/editing/v.txt");
	OUTPUT("", "hello from vi\n", "/bin/cat", "/tmp/editing/v.txt");
	STATUS(0, "/bin/test", "-d", "/var/tmp/vi.recover");
	// ex, from a script: substitute, append and write.
	OUTPUT("one\ntwo\n", "one\ntwo\n", "/bin/tee", "/tmp/editing/e.txt");
	OUTPUT("%s/o/0/g\n$a\nthree\n.\nwq\n", "", "/bin/ex", "-s", "/tmp/editing/e.txt");
	OUTPUT("", "0ne\ntw0\nthree\n", "/bin/cat", "/tmp/editing/e.txt");
	OUTPUT("2p\nq\n", "tw0\n", "/bin/ex", "-s", "/tmp/editing/e.txt");
	// view is vi that does not write the file.
	RUN_WITH(1, "s/0ne/1/\nw\nq!\n", "", 1, "/bin/view", "-e", "-s", "/tmp/editing/e.txt");
	OUTPUT("", "0ne\ntw0\nthree\n", "/bin/cat", "/tmp/editing/e.txt");
	STATUS(0, "/bin/rm", "-r", "/tmp/editing");
}

// tmux: a detached session, which starts the server; a command sent to
// its shell and what its pane then shows; a client attached at a
// terminal, which C-b d detaches; and the server's end. Client and
// server talk over a named socket, and the client passes its terminal to
// the server with SCM_RIGHTS (ADR 0091).
static void multiplexing(void) {
	CONTAINS("tmux 3.7c", "/bin/tmux", "-V");
	STATUS(0, "/bin/tmux", "-S", "/tmp/mux", "new-session", "-d", "-s", "work", "-x", "80", "-y",
			"24");
	CONTAINS("work: 1 windows", "/bin/tmux", "-S", "/tmp/mux", "list-sessions");
	STATUS(0, "/bin/tmux", "-S", "/tmp/mux", "send-keys", "-t", "work", "echo $((6 * 7))",
			"Enter");
	// The shell answers when it gets to it: look at the pane until it has.
	const char *const capture[] = {"/bin/tmux", "-S", "/tmp/mux", "capture-pane", "-p", "-t",
			"work", NULL};
	int shown = 0;
	for(int i = 0; i < 100 && !shown; i++) {
		shown = run_program(capture, "", 0) == 0 && strstr(output, "\n42\n");
		if(!shown)
			check_sleep_ms(50);
	}
	CHECK(shown);
	// Attached at a terminal, it draws the session's name in its status
	// line; C-b d detaches.
	TYPE_AT("[work]", "\002d", "/bin/tmux", "-S", "/tmp/mux", "attach-session", "-t", "work");
	CHECK(strstr(output, "detached") != NULL);
	STATUS(0, "/bin/tmux", "-S", "/tmp/mux", "kill-server");
	STATUS(1, "/bin/tmux", "-S", "/tmp/mux", "has-session");
	// Without -S, the socket is in /tmp/tmux-0, which tmux makes.
	STATUS(0, "/bin/tmux", "new-session", "-d");
	STATUS(0, "/bin/test", "-S", "/tmp/tmux-0/default");
	STATUS(0, "/bin/tmux", "kill-server");
}

// Every utility has its manual page.
static void manuals(void) {
	const char *const pages[] = {"ls", "cat", "sort", "test", "[", "find", "xargs", "grep", "sed",
			"awk", "diff", "cmp", "diff3", "sdiff", "patch", "bc", "dc", "less", "man", "mandoc",
			"apropos", "tput", "tset", "clear", "gzip", "gunzip", "zcat", "zgrep", "bzip2",
			"bunzip2", "bzcat", "xz", "unxz", "xzcat", "lzma", "xzgrep", "zstd", "unzstd",
			"zstdcat", "lz4", "unlz4", "tar", "cpio", "unzip", "bsdcat", "zip", "file", "jq",
			"nano", "rnano", "vi", "ex", "view", "tmux"};
	for(size_t i = 0; i < sizeof(pages) / sizeof(pages[0]); i++) {
		char path[64];
		snprintf(path, sizeof(path), "/usr/share/man/man1/%s.1", pages[i]);
		struct stat st;
		CHECK(stat(path, &st) == 0 && S_ISREG(st.st_mode) && st.st_size > 0);
	}
}

int main(void) {
	check_start("utilities");
	RUN(text);
	RUN(statuses);
	RUN(files);
	RUN(finding);
	RUN(searching);
	RUN(comparing);
	RUN(others);
	RUN(compressing);
	RUN(archiving);
	RUN(describing);
	RUN(editing);
	RUN(multiplexing);
	RUN(manuals);
	return check_end();
}
