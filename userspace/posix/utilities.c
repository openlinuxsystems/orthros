// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The ported utilities as a script uses them: each run through the links
// in /bin, with text on its standard input, and what it writes and its
// exit status checked. uutils' core utilities (ports/uutils-coreutils),
// find and xargs (ports/uutils-findutils), GNU grep, sed, diffutils and
// patch, awk, bc, less, ncurses' tools, mandoc's, the compressors and
// their scripts, tar, cpio, zip and its tools, 7-Zip, file and jq
// (ports/<name>), and their manual pages; the editors, nano and vi, and
// the terminal tools, typed at on a pseudo-terminal; tmux, its server and
// a client attached at a pseudo-terminal; git, in repositories on this
// computer; and the shell by each of its names. The `commands` test then checks that every program in
// /system/commands was run.

#include <dirent.h>
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
	OUTPUT("", "Dukovo\n", "/bin/uname", "-o");
	// The release is the kernel's version number, the second word of
	// "Radix 0.9.0 (Dukovo Server 0.9.0)", and the version the whole line.
	char line[64] = "", release[32] = "";
	FILE *version = fopen("/kernel/version", "r");
	CHECK(version && fgets(line, sizeof(line), version));
	fclose(version);
	CHECK(sscanf(line, "Radix %31s", release) == 1);
	char expected[40];
	snprintf(expected, sizeof(expected), "%s\n", release);
	OUTPUT("", expected, "/bin/uname", "-r");
	CONTAINS("Radix ", "/bin/uname", "-v");
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

// Checksums and encodings, of what the standards' own examples take.
static void checksums(void) {
	OUTPUT("abc", "a9993e364706816aba3e25717850c26c9cd0d89d  -\n", "/bin/sha1sum");
	OUTPUT("abc", "23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7  -\n",
			"/bin/sha224sum");
	OUTPUT("abc",
			"cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca1"
			"34c825a7  -\n",
			"/bin/sha384sum");
	OUTPUT("abc",
			"ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23"
			"a3feebbd454d4423643ce80e2a9ac94fa54ca49f  -\n",
			"/bin/sha512sum");
	OUTPUT("abc",
			"ba80a53f981c4d0d6a2797b69f12f6e94c212f14685ac4b74b12bb6fdbffa2d17d87c5392aab792dc252d5de"
			"4533cc9518d38aa8dbf1925ab92386edd4009923  -\n",
			"/bin/b2sum");
	OUTPUT("abc", "1219131554 3\n", "/bin/cksum");
	OUTPUT("abc", "16556     1\n", "/bin/sum");
	OUTPUT("hi", "NBUQ====\n", "/bin/base32");
	OUTPUT("NBUQ====\n", "hi", "/bin/base32", "-d");
	OUTPUT("hi", "6869\n", "/bin/basenc", "--base16");
}

// The filters that format text.
static void formatting(void) {
	OUTPUT("a\nb\n", "a b\n", "/bin/fmt");
	OUTPUT("abcd\n", "ab\ncd\n", "/bin/fold", "-w", "2");
	OUTPUT("a\nb\n", "     1\ta\n     2\tb\n", "/bin/nl");
	OUTPUT("a\nb\n", "a,b\n", "/bin/paste", "-s", "-d,");
	OUTPUT("        a\n", "\ta\n", "/bin/unexpand");
	OUTPUT("a b\nb c\n", "a\nb\nc\n", "/bin/tsort");
	OUTPUT("a\n", "a\n", "/bin/pr", "-t");
	RUN_WITH(0, "a b\n", "a b", 1, "/bin/ptx");
	OUTPUT("", "1.0M\n", "/bin/numfmt", "--to=iec", "1048576");
	OUTPUT("", "y\ny\n", "/bin/sh", "-c", "yes | head -n 2");
	OUTPUT("", "1\n2\n3\n", "/bin/sh", "-c", "shuf -i 1-3 | sort -n");
	STATUS(0, "/bin/mkdir", "-p", "/tmp/formatting");
	OUTPUT("1 a\n2 b\n", "1 a\n2 b\n", "/bin/tee", "/tmp/formatting/one");
	OUTPUT("1 x\n2 y\n", "1 x\n2 y\n", "/bin/tee", "/tmp/formatting/two");
	OUTPUT("", "1 a x\n2 b y\n", "/bin/join", "/tmp/formatting/one", "/tmp/formatting/two");
	OUTPUT("", "\t\t1 a\n\t\t2 b\n", "/bin/comm", "/tmp/formatting/one",
			"/tmp/formatting/one");
	STATUS(0, "/bin/rm", "-r", "/tmp/formatting");
}

// The rest of the file tools: copies by blocks, sizes, installs, times,
// links, named pipes, empty directories, temporary names, pieces,
// overwriting, listings and their colours, and portable names.
static void file_tools(void) {
	STATUS(0, "/bin/mkdir", "-p", "/tmp/file-tools/listed");
	OUTPUT("abc", "abc", "/bin/dd", "status=none");
	OUTPUT("", "8\n", "/bin/sh", "-c", "dd if=/devices/zero bs=4 count=2 status=none | wc -c");
	STATUS(0, "/bin/truncate", "-s", "10", "/tmp/file-tools/t");
	OUTPUT("", "10\n", "/bin/stat", "-c", "%s", "/tmp/file-tools/t");
	STATUS(0, "/bin/install", "-m", "644", "-D", "/tmp/file-tools/t", "/tmp/file-tools/i/t");
	OUTPUT("", "10\n", "/bin/stat", "-c", "%s", "/tmp/file-tools/i/t");
	STATUS(0, "/bin/unlink", "/tmp/file-tools/i/t");
	STATUS(1, "/bin/test", "-e", "/tmp/file-tools/i/t");
	// Times that touch sets, and that cp -p keeps.
	STATUS(0, "/bin/touch", "-d", "@1000000000", "/tmp/file-tools/t");
	OUTPUT("", "1000000000\n", "/bin/stat", "-c", "%Y", "/tmp/file-tools/t");
	STATUS(0, "/bin/cp", "-p", "/tmp/file-tools/t", "/tmp/file-tools/kept");
	OUTPUT("", "1000000000\n", "/bin/stat", "-c", "%Y", "/tmp/file-tools/kept");
	// Hard links, named pipes and device nodes (ADR 0135): a reader and
	// a writer meet through the pipe, and the node writes to the null
	// device.
	STATUS(0, "/bin/link", "/tmp/file-tools/t", "/tmp/file-tools/l");
	STATUS(0, "/bin/ln", "/tmp/file-tools/t", "/tmp/file-tools/l2");
	OUTPUT("", "3 1000000000\n", "/bin/stat", "-c", "%h %Y", "/tmp/file-tools/l2");
	STATUS(0, "/bin/rm", "/tmp/file-tools/l", "/tmp/file-tools/l2");
	OUTPUT("", "1\n", "/bin/stat", "-c", "%h", "/tmp/file-tools/t");
	STATUS(0, "/bin/mkfifo", "/tmp/file-tools/fifo");
	OUTPUT("", "fifo\n", "/bin/stat", "-c", "%F", "/tmp/file-tools/fifo");
	OUTPUT("", "through\n", "/bin/sh", "-c",
			"echo through > /tmp/file-tools/fifo & cat /tmp/file-tools/fifo; wait");
	STATUS(0, "/bin/mknod", "/tmp/file-tools/node", "c", "1", "3");
	OUTPUT("", "character special file 1 3\n", "/bin/stat", "-c", "%F %t %T",
			"/tmp/file-tools/node");
	OUTPUT("", "", "/bin/sh", "-c", "echo lost > /tmp/file-tools/node; cat /tmp/file-tools/node");
	STATUS(0, "/bin/rm", "/tmp/file-tools/fifo", "/tmp/file-tools/node");
	CONTAINS("/tmp/file-tools/x.", "/bin/mktemp", "/tmp/file-tools/x.XXXXXX");
	CONTAINS("/tmp/file-tools/d.", "/bin/mktemp", "-d", "/tmp/file-tools/d.XXXXXX");
	OUTPUT("1\n2\n3\n", "", "/bin/split", "-l", "1", "-", "/tmp/file-tools/part.");
	OUTPUT("", "2\n", "/bin/cat", "/tmp/file-tools/part.ab");
	OUTPUT("a\nb\nc\n", "2\n4\n", "/bin/csplit", "-f", "/tmp/file-tools/piece", "-", "2");
	OUTPUT("", "b\nc\n", "/bin/cat", "/tmp/file-tools/piece01");
	STATUS(0, "/bin/shred", "-u", "/tmp/file-tools/part.aa");
	STATUS(1, "/bin/test", "-e", "/tmp/file-tools/part.aa");
	STATUS(0, "/bin/sync");
	STATUS(0, "/bin/mkdir", "/tmp/file-tools/empty");
	STATUS(0, "/bin/rmdir", "/tmp/file-tools/empty");
	STATUS(1, "/bin/test", "-e", "/tmp/file-tools/empty");
	STATUS(0, "/bin/touch", "/tmp/file-tools/listed/a", "/tmp/file-tools/listed/b");
	OUTPUT("", "a  b\n", "/bin/dir", "/tmp/file-tools/listed");
	CONTAINS(" b\n", "/bin/vdir", "/tmp/file-tools/listed");
	CONTAINS("LS_COLORS='", "/bin/dircolors", "-b");
	OUTPUT("", "", "/bin/pathchk", "-p", "a/b");
	STATUS(1, "/bin/pathchk", "-p", "a*");
	STATUS(0, "/bin/rm", "-r", "/tmp/file-tools");
}

// Who and where: the user, the machine, the working directory, a
// priority and a hang-up, and the program behind the links. Every
// process is user 0 until Phase 13 (radix-posix(7)).
static void identity(void) {
	OUTPUT("", "root\n", "/bin/whoami");
	OUTPUT("", "0\n", "/bin/id", "-u");
	CONTAINS("uid=0(root) gid=0(root)", "/bin/id");
	OUTPUT("", "root\n", "/bin/groups");
	// No login records until Phase 13, so no login name.
	RUN_WITH(1, "", "no login name", 1, "/bin/sh", "-c", "logname 2>&1");
	STATUS(0, "/bin/hostname");
	OUTPUT("", "x86_64\n", "/bin/arch");
	OUTPUT("", "4\n", "/bin/nproc");
	OUTPUT("", "/tmp\n", "/bin/sh", "-c", "cd /tmp && /bin/pwd");
	OUTPUT("", "3\n", "/bin/nice", "-n", "3", "/bin/nice");
	OUTPUT("", "hi\n", "/bin/nohup", "/bin/echo", "hi");
	OUTPUT("", "hi\n", "/system/commands/coreutils", "echo", "hi");
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
	OUTPUT("", "/system/commands\n", "/bin/realpath", "/usr/bin");
	// The root's Unix names are links its .hidden lists (ADR 0113): ls
	// leaves them out unless asked for every name.
	OUTPUT("", "applications\ndevices\nkernel\nprocesses\nshared\nsystem\ntmp\nusers\nvolumes\n",
			"/bin/ls", "/");
	OUTPUT("", ".hidden\napplications\nbin\ndev\ndevices\netc\nkernel\nproc\nprocesses\nrun\n"
			   "sbin\nshared\nsys\nsystem\ntmp\nusers\nusr\nvar\nvolumes\n",
			"/bin/ls", "-A", "/");
	OUTPUT("", "/dev\n", "/bin/ls", "-d", "/dev");
	// So are those of /system (ADR 0114).
	OUTPUT("", "boot\ncommands\nlibraries\nlive\nresources\nsettings\nstate\ntests\n", "/bin/ls",
			"/system");
	OUTPUT("", "/system/libraries\n", "/bin/realpath", "/usr/lib");
	CONTAINS("-> system/commands", "/bin/ls", "-ld", "/bin");
	CONTAINS("coreutils\n", "/bin/ls", "/system/commands");
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
	OUTPUT("", "/processes\n", "/bin/find", "/processes", "-maxdepth", "0", "-fstype", "proc");
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
	OUTPUT("", "/system/resources/man/man1/grep.1\n", "/bin/man", "-w", "grep");
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

	// The other scripts: the other greps, cmp and diff, and the pagers,
	// which show the text as it is when their output is not a terminal.
	STATUS(0, "/bin/sh", "-c", "cd /tmp/compress && gzip -k f && lzma -k f");
	OUTPUT("", "needle\n", "/bin/zegrep", "ne+dle", "/tmp/compress/f.gz");
	OUTPUT("", "needle\n", "/bin/zfgrep", "needle", "/tmp/compress/f.gz");
	OUTPUT("", "needle\n", "/bin/bzegrep", "ne+dle", "/tmp/compress/f.bz2");
	OUTPUT("", "needle\n", "/bin/bzfgrep", "needle", "/tmp/compress/f.bz2");
	OUTPUT("", "needle\n", "/bin/xzegrep", "ne+dle", "/tmp/compress/f.xz");
	OUTPUT("", "needle\n", "/bin/xzfgrep", "needle", "/tmp/compress/f.xz");
	OUTPUT("", "needle\n", "/bin/lzgrep", "needle", "/tmp/compress/f.lzma");
	OUTPUT("", "needle\n", "/bin/lzegrep", "ne+dle", "/tmp/compress/f.lzma");
	OUTPUT("", "needle\n", "/bin/lzfgrep", "needle", "/tmp/compress/f.lzma");
	STATUS(0, "/bin/zdiff", "/tmp/compress/f.gz", "/tmp/compress/f");
	STATUS(0, "/bin/bzcmp", "/tmp/compress/f.bz2", "/tmp/compress/f");
	STATUS(0, "/bin/bzdiff", "/tmp/compress/f.bz2", "/tmp/compress/f");
	STATUS(0, "/bin/xzcmp", "/tmp/compress/f.xz", "/tmp/compress/f");
	STATUS(0, "/bin/lzcmp", "/tmp/compress/f.lzma", "/tmp/compress/f");
	STATUS(0, "/bin/lzdiff", "/tmp/compress/f.lzma", "/tmp/compress/f");
	OUTPUT("", "one\nneedle\n", "/bin/lzcat", "/tmp/compress/f.lzma");
	CONTAINS("Uncompressed size:", "/bin/lzmainfo", "/tmp/compress/f.lzma");
	CONTAINS("needle", "/bin/zless", "/tmp/compress/f.gz");
	CONTAINS("needle", "/bin/zmore", "/tmp/compress/f.gz");
	CONTAINS("needle", "/bin/bzless", "/tmp/compress/f.bz2");
	CONTAINS("needle", "/bin/bzmore", "/tmp/compress/f.bz2");
	CONTAINS("needle", "/bin/xzless", "/tmp/compress/f.xz");
	CONTAINS("needle", "/bin/xzmore", "/tmp/compress/f.xz");
	CONTAINS("needle", "/bin/lzless", "/tmp/compress/f.lzma");
	CONTAINS("needle", "/bin/lzmore", "/tmp/compress/f.lzma");
	CONTAINS("needle", "/bin/zstdless", "/tmp/compress/f.zst");
	// The other compressors' names, and bzip2's repair of a damaged file
	// from its blocks.
	OUTPUT("text\n", "text\n", "/bin/sh", "-c", "lz4c | lz4 -d");
	OUTPUT("text\n", "text\n", "/bin/sh", "-c", "zstdmt | zstd -d");
	STATUS(0, "/bin/bzip2recover", "/tmp/compress/f.bz2");
	OUTPUT("", "one\nneedle\n", "/bin/bzcat", "/tmp/compress/rec00001f.bz2");
	// zforce names a gzip file as one; compress's format, which gzip
	// reads and znew makes a gzip file of.
	STATUS(0, "/bin/sh", "-c", "gzip -c /tmp/compress/f >/tmp/compress/z");
	STATUS(0, "/bin/zforce", "/tmp/compress/z");
	OUTPUT("", "one\nneedle\n", "/bin/zcat", "/tmp/compress/z.gz");
	STATUS(0, "/bin/sh", "-c",
			"printf '\\037\\235\\220\\164\\312\\340\\241\\243\\000' >/tmp/compress/t.Z");
	OUTPUT("", "text\n", "/bin/uncompress", "-c", "/tmp/compress/t.Z");
	STATUS(0, "/bin/znew", "/tmp/compress/t.Z");
	OUTPUT("", "text\n", "/bin/zcat", "/tmp/compress/t.gz");
	// gzexe makes a program that unpacks and runs itself.
	STATUS(0, "/bin/cp", "/system/tests/posix/hello", "/tmp/compress/hello");
	STATUS(0, "/bin/gzexe", "/tmp/compress/hello");
	OUTPUT("", "#!/bin/sh\n", "/bin/sed", "1q", "/tmp/compress/hello");
	CONTAINS("a megabyte from malloc", "/tmp/compress/hello");
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
	// libarchive's programs by their own names, and zipsplit's pieces.
	OUTPUT("", "d/\nd/a\nd/e/\nd/e/b\nd/l\n", "/bin/sh", "-c",
			"bsdtar -tf /tmp/archive/t.tar | sort");
	OUTPUT("", "d\nd/a\nd/e\nd/e/b\nd/l\n", "/bin/sh", "-c",
			"bsdcpio -it --quiet </tmp/archive/t.cpio | sort");
	OUTPUT("", "first\n", "/bin/bsdunzip", "-p", "/tmp/archive/u.zip", "d/a");
	STATUS(0, "/bin/zipsplit", "-q", "-n", "100000", "-b", "/tmp/archive", "/tmp/archive/t.zip");
	OUTPUT("", "second\n", "/bin/unzip", "-p", "/tmp/archive/t1.zip", "d/e/b");
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
	CONTAINS("ELF 64-bit LSB pie executable, x86-64", "/bin/file", "-b", "/system/commands/jq");
	CONTAINS("directory", "/bin/file", "/tmp/describe");
	CONTAINS("symbolic link to system/commands", "/bin/file", "/bin");
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

// mandoc's tools on a page of their own, less's helpers, a terminal
// description compiled, and the other names of grep and less.
static void documents(void) {
	STATUS(0, "/bin/mkdir", "-p", "/tmp/documents/man/man1");
	OUTPUT(".TH DUKE 1\n.SH NAME\nduke \\- a page for the tools\n.SH DESCRIPTION\nText.\n",
			".TH DUKE 1\n.SH NAME\nduke \\- a page for the tools\n.SH DESCRIPTION\nText.\n",
			"/bin/tee", "/tmp/documents/man/man1/duke.1");
	CONTAINS("a page for the tools", "/bin/mandoc", "-T", "ascii", "/tmp/documents/man/man1/duke.1");
	STATUS(0, "/bin/mandoc", "-T", "lint", "-W", "error", "/tmp/documents/man/man1/duke.1");
	CONTAINS("a page for the tools", "/bin/demandoc", "/tmp/documents/man/man1/duke.1");
	RUN_WITH(0, ".so /tmp/documents/man/man1/duke.1\n", "a page for the tools", 1, "/bin/soelim");
	STATUS(0, "/bin/makewhatis", "/tmp/documents/man");
	CONTAINS("duke(1) - a page for the tools", "/bin/whatis", "-M", "/tmp/documents/man", "duke");
	OUTPUT("", "a b\n", "/bin/lessecho", "a", "b");
	RUN_WITH(0, "#command\nQ quit\n", "", 1, "/bin/lesskey", "-o", "/tmp/documents/keys", "-");
	STATUS(0, "/bin/test", "-s", "/tmp/documents/keys");
	OUTPUT("duke|a test terminal,\n\tcols#80, lines#24,\n",
			"duke|a test terminal,\n\tcols#80, lines#24,\n", "/bin/tee", "/tmp/documents/duke.ti");
	STATUS(0, "/bin/tic", "-o", "/tmp/documents/terminfo", "/tmp/documents/duke.ti");
	OUTPUT("", "80\n", "/bin/env", "TERMINFO=/tmp/documents/terminfo", "/bin/tput", "-T", "duke",
			"cols");
	OUTPUT("a.b\naxb\n", "a.b\n", "/bin/fgrep", "a.b");
	OUTPUT("line\n", "line\n", "/bin/more");
	STATUS(0, "/bin/rm", "-r", "/tmp/documents");
}

// Runs `argv` on a new pseudo-terminal of 24 lines of 80 columns, with
// TERM=xterm, as someone at a terminal would: waits until it has written
// `ready`, which it does once it has drawn its screen and so takes keys
// as they are typed, and has turned echo off, as it does before reading
// keys, types `keys`, and reads what it writes until it ends, within 20
// seconds. A program that asks more than once is typed at in steps: each
// line of `ready` it writes gets it the line of `keys` that goes with
// it. Returns its exit status, or -1 if it did not get that far; what it
// wrote is in `output`, as much as that holds.
static int on_terminal(const char *const argv[], const char *ready, const char *keys) {
	note_commands(argv);
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
	// The terminal side, to see its modes by: a program that turns echo
	// off throws away what was typed before (TCSAFLUSH).
	int terminal = open(name, O_RDWR | O_NOCTTY | O_CLOEXEC);
	struct timespec now, end;
	clock_gettime(CLOCK_MONOTONIC, &end);
	end.tv_sec += 20;
	size_t len = 0, seen = 0;
	const char *wait_for = ready, *to_type = keys;
	int typed = 0, failed = 0, status = -1;
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
		while(!typed) {
			char step[128];
			size_t want = strcspn(wait_for, "\n");
			snprintf(step, sizeof(step), "%.*s", (int)want, wait_for);
			const char *found = strstr(output + seen, step);
			if(!found)
				break;
			size_t count = strcspn(to_type, "\n");
			struct termios modes;
			if(count && (terminal < 0 || tcgetattr(terminal, &modes) < 0 || (modes.c_lflag & ECHO)))
				break;
			seen = (size_t)(found - output) + want;
			if(write(master, to_type, count) != (ssize_t)count) {
				failed = 1;
				break;
			}
			wait_for += want + (wait_for[want] == '\n');
			to_type += count;
			typed = !*to_type;
			to_type += !typed;
		}
		if(failed)
			break;
		if(typed && terminal >= 0) {
			close(terminal);
			terminal = -1;
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
	if(terminal >= 0)
		close(terminal);
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
	// rnano, nano that writes only the file it was given.
	TYPE_AT("r.txt", "restricted\023\021", "/bin/rnano", "/tmp/editing/r.txt");
	OUTPUT("", "restricted\n", "/bin/cat", "/tmp/editing/r.txt");
	STATUS(0, "/bin/rm", "-r", "/tmp/editing");
}

// The terminal tools at a terminal, and what tty says without one; and
// zipcloak, which asks for its password there.
static void terminals(void) {
	TYPE_AT("24 80", "", "/bin/stty", "size");
	TYPE_AT("/devices/pts/", "", "/bin/tty");
	STATUS(1, "/bin/tty");
	TYPE_AT("xterm", "", "/bin/tset", "-q");
	// xterm's full reset, and its clearing of the screen.
	TYPE_AT("\033c", "", "/bin/reset");
	TYPE_AT("\033[2J", "", "/bin/clear");
	STATUS(0, "/bin/mkdir", "-p", "/tmp/terminals");
	OUTPUT("secret\n", "secret\n", "/bin/tee", "/tmp/terminals/s");
	STATUS(0, "/bin/sh", "-c", "cd /tmp/terminals && zip -q t.zip s");
	// It asks twice, and throws away what was typed before each question,
	// as programs that turn off echo with TCSAFLUSH do.
	TYPE_AT("Enter password\nVerify password", "word\r\nword\r", "/bin/zipcloak",
			"/tmp/terminals/t.zip");
	OUTPUT("", "secret\n", "/bin/unzip", "-P", "word", "-p", "/tmp/terminals/t.zip", "s");
	CHECK(run_program((const char *const[]){"/bin/unzip", "-P", "wrong", "-p",
						  "/tmp/terminals/t.zip", "s", NULL},
				  "", 0) != 0);
	STATUS(0, "/bin/rm", "-r", "/tmp/terminals");
}

// The shell by its other names: as bash, which says it is bash 5.2
// (ADR 0103); as sh, which does not; and restricted, as rnukesh.
static void shells(void) {
	OUTPUT("", "5.2.0(1)-release\n", "/bin/bash", "-c", "echo $BASH_VERSION");
	OUTPUT("", "yes\n", "/bin/bash", "-c", "[[ abc == a* ]] && echo yes");
	OUTPUT("", "unset\n", "/bin/sh", "-c", "echo ${BASH_VERSION-unset}");
	OUTPUT("", "yes\n", "/system/commands/nukesh", "-c", "[ -n \"$NUKESH_VERSION\" ] && echo yes");
	OUTPUT("", "hi\n", "/bin/rnukesh", "-c", "echo hi");
	STATUS(1, "/bin/rnukesh", "-c", "cd /");
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

// Every program in /system/commands has its manual page, which man finds by
// the program's name, and its completion file for the shell; ob checks
// both when it builds the system (ADR 0102).
// git (ports/git) as a person and a script use it, in repositories on
// this computer: the commits' names are the hashes of what they hold, so
// with the names and times set they are the same as on any system.
static void versioning(void) {
	static const char *const ENV[][2] = {{"HOME", "/tmp/git/home"},
			{"GIT_AUTHOR_NAME", "Dukovo"}, {"GIT_AUTHOR_EMAIL", "test@dukovo.org"},
			{"GIT_COMMITTER_NAME", "Dukovo"}, {"GIT_COMMITTER_EMAIL", "test@dukovo.org"},
			{"GIT_AUTHOR_DATE", "2026-11-01T12:00:00Z"},
			{"GIT_COMMITTER_DATE", "2026-11-01T12:00:00Z"}};
	const char *home = getenv("HOME");
	char *was_home = home ? strdup(home) : NULL;
	for(size_t i = 0; i < sizeof(ENV) / sizeof(ENV[0]); i++)
		setenv(ENV[i][0], ENV[i][1], 1);
	STATUS(0, "/bin/mkdir", "-p", "/tmp/git/home");
	CONTAINS("git version 2.55.0", "/bin/git", "--version");
	OUTPUT("", "refs/heads/main\n", "/bin/sh", "-c",
			"git init -q /tmp/git/a && git -C /tmp/git/a symbolic-ref HEAD");
	OUTPUT("", "cc06e7d412c7d2cc7fccdd86964e3543048ab5e6 First\n", "/bin/sh", "-c",
			"cd /tmp/git/a && printf 'one\\ntwo\\nthree\\n' >f && git add f && git commit -q -m First && git log --format='%H %s'");
	OUTPUT("", " M f\n f | 1 +\n 1 file changed, 1 insertion(+)\n", "/bin/sh", "-c",
			"cd /tmp/git/a && echo four >>f && git status --short && git diff --stat");
	// Branches, and a merge of changes to different lines.
	OUTPUT("", "Auto-merging f\n*   Merge branch 'side'\n|\\  \n| * Side\n* | Third\n|/  \n* Second\n* First\n", "/bin/sh", "-c",
			"cd /tmp/git/a && git commit -q -am Second && git switch -q -c side && sed -i s/one/ONE/ f && git commit -q -am Side && git switch -q main && echo five >>f && git commit -q -am Third && git merge -q --no-edit side && git log --graph --format=%s");
	OUTPUT("", "ONE\ntwo\nthree\nfour\nfive\ne13c7142024f8ae6e85d057d52da8e8efcd835f5\n", "/bin/sh", "-c",
			"cd /tmp/git/a && cat f && git rev-parse HEAD");
	OUTPUT("", "v1\nv1\n", "/bin/sh", "-c",
			"cd /tmp/git/a && git tag -a -m 'Version 1' v1 && git describe && git tag");
	OUTPUT("", " M f\n", "/bin/sh", "-c",
			"cd /tmp/git/a && echo six >>f && git stash -q && git status --short && git stash pop -q && git status --short && git checkout -q f && git status --short");
	OUTPUT("", "f:1:ONE\n1) ONE\n2) two\n3) three\n4) four\n5) five\n", "/bin/sh", "-c",
			"cd /tmp/git/a && git grep -n ONE && git blame -s f | cut -c10-");
	// A conflict: the merge stops, names the file, and is undone.
	OUTPUT("", "1\nf\n", "/bin/sh", "-c",
			"cd /tmp/git/a && git switch -q -c clash HEAD~1 && sed -i s/one/uno/ f && git commit -q -am Clash && git switch -q main && { git merge -q clash >/dev/null 2>&1; echo $?; } && git diff --name-only --diff-filter=U && git merge --abort && git status --short");
	// Cloning through the file:// transport, which runs git-upload-pack, and pushing, which
	// runs git-receive-pack.
	OUTPUT("", "Merge branch 'side'\n  origin/HEAD -> origin/main\n  origin/clash\n  origin/main\n  origin/side\n", "/bin/sh", "-c",
			"git clone -q file:///tmp/git/a /tmp/git/b && git -C /tmp/git/b log -1 --format=%s && git -C /tmp/git/b branch -r");
	OUTPUT("", "Fourth\n", "/bin/sh", "-c",
			"git clone -q --bare /tmp/git/a /tmp/git/c.git && cd /tmp/git/b && echo seven >>f && git commit -q -am Fourth && git push -q /tmp/git/c.git main && git -C /tmp/git/c.git log -1 --format=%s");
	// Packing and checking every object.
	OUTPUT("", "count: 0\nin-pack: 25\n", "/bin/sh", "-c",
			"cd /tmp/git/a && git gc -q && git fsck --no-progress --no-dangling && git count-objects -v | grep -E '^(count|in-pack)'");
	// Shell scripts of git's: submodule, and bisect running a test.
	OUTPUT("", " sub (v1-1-g276f6ce)\n.gitmodules\nf\nsub\n", "/bin/sh", "-c",
			"cd /tmp/git/a && git -c protocol.file.allow=always submodule add -q /tmp/git/c.git sub && git submodule status | cut -c42- && git commit -q -m Sub && git ls-files");
	OUTPUT("", "a548972b3431cc1d69aa17933741440a41c54b23 is the first 'bad' commit\nbisect found first 'bad' commit\nmain\n", "/bin/sh", "-c",
			"cd /tmp/git/a && git bisect start HEAD v1~2 >/dev/null && git bisect run sh -c '! grep -q five f' | grep 'first .bad. commit' ; git bisect reset >/dev/null 2>&1; git branch --show-current");
	// The programs another computer runs by name: the refs each offers, git-shell letting only
	// them run, and an archive asked for over the protocol.
	OUTPUT("", "refs/heads/clash\n", "/bin/sh", "-c",
			"git-upload-pack --advertise-refs /tmp/git/a | head -2 | tail -1 | cut -c46-");
	OUTPUT("", "refs/heads/clash\n", "/bin/sh", "-c",
			"git-receive-pack --advertise-refs /tmp/git/c.git | head -1 | cut -c46-61");
	OUTPUT("", "refs/heads/main\n", "/bin/sh", "-c",
			"git-shell -c \"git-upload-pack '/tmp/git/a'\" </dev/null 2>/dev/null | head -3 | tail -1 | cut -c46-");
	STATUS(128, "/bin/sh", "-c",
			"git-shell -c ls 2>/dev/null");
	OUTPUT("", "f\n", "/bin/sh", "-c",
			"git archive --remote=/tmp/git/a --format=tar v1 | tar -tf -");
	OUTPUT("", "0008ACK\n0000", "/bin/sh", "-c",
			"printf '0012argument HEAD\\n0000' | git-upload-archive /tmp/git/a | head -c 12");
	// Its manual pages, which `git help` shows.
	OUTPUT("", "/system/resources/man/man1/git-commit.1\n/system/resources/man/man7/gitglossary.7\n",
			"/bin/man", "-w", "git-commit", "gitglossary");
	STATUS(0, "/bin/rm", "-rf", "/tmp/git");
	for(size_t i = 0; i < sizeof(ENV) / sizeof(ENV[0]); i++)
		unsetenv(ENV[i][0]);
	if(was_home) {
		setenv("HOME", was_home, 1);
		free(was_home);
	}
}

static void manuals(void) {
	DIR *bin = opendir("/system/commands");
	CHECK(bin != NULL);
	int programs = 0;
	struct dirent *entry;
	while((entry = readdir(bin))) {
		if(entry->d_name[0] == '.')
			continue;
		const char *const argv[] = {"/bin/man", "-w", entry->d_name, NULL};
		int status = run_program(argv, "", 0);
		char completions[128];
		snprintf(completions, sizeof(completions), "/system/resources/nukesh/completions/%s",
				entry->d_name);
		struct stat st;
		if(status != 0 || !strstr(output, "/resources/man/man") || stat(completions, &st) != 0) {
			char what[192];
			snprintf(what, sizeof(what), "man -w %s exited with %d and wrote \"%.100s\"",
					entry->d_name, status, output);
			check_failed(__FILE__, __LINE__, what, 0);
		}
		programs++;
	}
	closedir(bin);
	CHECK(programs > 200);
	// A page is found by each name its NAME section gives, through the
	// index makewhatis made, which whatis and apropos read too.
	CONTAINS("/resources/man/man8/reboot.8", "/bin/man", "-w", "poweroff");
	CONTAINS("poweroff(8) - restart, power off or halt the machine", "/bin/whatis", "poweroff");
	CONTAINS("nukesh", "/bin/apropos", "shell");
	CONTAINS("the 7-Zip archiver", "/bin/sh", "-c", "man 7z | head -n 8");
	CONTAINS("Dukovo's shell", "/bin/sh", "-c", "man sh | head -n 8");
	// The options of a program's completion file come from its page.
	CONTAINS("option\t-i --ignore-case\t", "/bin/cat", "/system/resources/nukesh/completions/grep");
}

int main(void) {
	check_start("utilities");
	RUN(text);
	RUN(statuses);
	RUN(checksums);
	RUN(formatting);
	RUN(file_tools);
	RUN(identity);
	RUN(files);
	RUN(finding);
	RUN(searching);
	RUN(comparing);
	RUN(others);
	RUN(compressing);
	RUN(archiving);
	RUN(describing);
	RUN(documents);
	RUN(editing);
	RUN(terminals);
	RUN(multiplexing);
	RUN(versioning);
	RUN(shells);
	RUN(manuals);
	return check_end();
}
