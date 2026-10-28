// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The native administration tools (systools, ADR 0096) as an
// administrator's commands and scripts use them, each through its link
// in /bin: the process tools and their signals, sessions, memory and
// disks, priorities, CPU affinity and I/O classes, held files, traces,
// the kernel log, the dump and layout tools, script, watch and pidstat,
// and shutdown and service through Spark. With them, what the kernel
// tells them: /processes (radix-proc(5)), /devices/kmsg, getloadavg, and the
// priorities a child inherits.

#include <fcntl.h>
#include <sys/resource.h>
#include <sys/stat.h>

#include "check.h"
#include "programs.h"

// Where the test keeps its files.
#define DIR "/tmp/admin"

// What `path` holds, NUL-terminated, in `buffer` of `size` bytes; or
// an empty string.
static const char *read_file(const char *path, char *buffer, size_t size) {
	buffer[0] = 0;
	int fd = open(path, O_RDONLY | O_CLOEXEC);
	if(fd < 0)
		return buffer;
	size_t len = 0;
	ssize_t got;
	while(len < size - 1 && (got = read(fd, buffer + len, size - 1 - len)) > 0)
		len += got;
	buffer[len] = 0;
	close(fd);
	return buffer;
}

// Writes `text` to `path`, replacing what it held.
static void write_file(const char *path, const char *text) {
	int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0644);
	CHECK(fd >= 0);
	if(fd < 0)
		return;
	CHECK(write(fd, text, strlen(text)) == (ssize_t)strlen(text));
	close(fd);
}

// Starts `/bin/sleep 30`, a child to look at and signal, and waits
// until it runs sleep: until then it is still called admin.
static pid_t start_sleeper(void) {
	pid_t pid = fork();
	if(!pid) {
		execl("/bin/sleep", "sleep", "30", (char *)NULL);
		_exit(127);
	}
	CHECK(pid > 0);
	char path[64], text[4096];
	snprintf(path, sizeof(path), "/processes/%d/status", pid);
	long long deadline = check_now_ns() + 5000000000LL;
	while(!strstr(read_file(path, text, sizeof(text)), "\nname: sleep\n") &&
			check_now_ns() < deadline)
		check_sleep_ms(5);
	CHECK(strstr(text, "\nname: sleep\n") != NULL);
	return pid;
}

// Ends a child from start_sleeper.
static void end_sleeper(pid_t pid) {
	kill(pid, SIGKILL);
	waitpid(pid, NULL, 0);
}

// `number` as text, in `buffer` of 24 bytes.
static const char *text_of(long number, char *buffer) {
	snprintf(buffer, 24, "%ld", number);
	return buffer;
}

// The foreground process group of this process's terminal.
static pid_t foreground(void) {
	int tty = open("/devices/tty", O_RDONLY | O_CLOEXEC);
	CHECK(tty >= 0);
	pid_t group = tcgetpgrp(tty);
	close(tty);
	CHECK(group > 0);
	return group;
}

// What /processes says of processes: this one's status, arguments and
// threads, and the system's CPUs and load.
static void proc(void) {
	char path[64], text[4096], line[64];
	snprintf(path, sizeof(path), "/processes/%d/status", getpid());
	read_file(path, text, sizeof(text));
	snprintf(line, sizeof(line), "\nparent: %d\n", getppid());
	CHECK(strstr(text, line) != NULL);
	snprintf(line, sizeof(line), "\ngroup: %d\n", getpgrp());
	CHECK(strstr(text, line) != NULL);
	CHECK(strstr(text, "\nterminal: pts/") != NULL);
	snprintf(line, sizeof(line), "\nterminal_group: %d\n", foreground());
	CHECK(strstr(text, line) != NULL);
	CHECK(strstr(text, "\nio_class: none\n") != NULL);
	CHECK(strstr(text, "\ntraced: no\n") != NULL);
	CHECK(strstr(text, "\nuser_ns: ") != NULL);
	snprintf(path, sizeof(path), "/processes/%d/cmdline", getpid());
	int fd = open(path, O_RDONLY | O_CLOEXEC);
	CHECK(fd >= 0);
	ssize_t got = read(fd, text, sizeof(text));
	close(fd);
	static const char ARGS[] = "/system/tests/posix/admin";
	CHECK(got == sizeof(ARGS) && !memcmp(text, ARGS, sizeof(ARGS)));
	snprintf(path, sizeof(path), "/processes/%d/threads", getpid());
	read_file(path, text, sizeof(text));
	CHECK(strstr(text, "name=") != NULL && strchr(text, '\n') == text + strlen(text) - 1);
	read_file("/processes/stat", text, sizeof(text));
	CHECK(strstr(text, "\ncpus: 4\n") != NULL);
	read_file("/processes/loadavg", text, sizeof(text));
	CHECK(!strncmp(text, "load_1: ", 8));
	double loads[3] = {-1, -1, -1};
	CHECK(getloadavg(loads, 3) == 3);
	CHECK(loads[0] >= 0 && loads[1] >= 0 && loads[2] >= 0);
}

// ps, top, pgrep, pkill and kill: the processes as they are, a stopped
// one, and signals sent by name.
static void processes(void) {
	char me[24];
	text_of(getpid(), me);
	OUTPUT("", "spark\n", "/bin/ps", "-o", "comm=", "-p", "1");
	CHECK(run_program((const char *const[]){"/bin/ps", "-o", "ppid=", "-p", me, NULL}, "", 0) == 0);
	CHECK(atol(output) == getppid());
	// TPGID is the terminal's foreground group, and STAT has a + when
	// that is this process's group.
	CHECK(run_program((const char *const[]){"/bin/ps", "-o", "tpgid=", "-p", me, NULL}, "", 0) == 0);
	CHECK(atol(output) == foreground());
	CHECK(run_program((const char *const[]){"/bin/ps", "-o", "stat=", "-p", me, NULL}, "", 0) == 0);
	CHECK((strchr(output, '+') != NULL) == (foreground() == getpgrp()));
	CONTAINS(" admin\n", "/bin/ps", "-e");
	CONTAINS("USER", "/bin/ps", "aux");
	CONTAINS("root", "/bin/ps", "aux");
	CONTAINS("spark", "/bin/ps", "-ef", "--forest");
	STATUS(1, "/bin/ps", "-p", "999999");
	CONTAINS("load average:", "/bin/top", "-b", "-n", "1");
	CONTAINS("PID USER", "/bin/top", "-b", "-n", "1");

	// A stopped child shows as T.
	pid_t stopped = fork();
	if(!stopped) {
		raise(SIGSTOP);
		_exit(0);
	}
	int status;
	CHECK(waitpid(stopped, &status, WUNTRACED) == stopped && WIFSTOPPED(status));
	char number[24];
	CHECK(run_program((const char *const[]){"/bin/ps", "-o", "stat=", "-p",
						  text_of(stopped, number), NULL},
				  "", 0) == 0);
	CHECK(output[0] == 'T');
	STATUS(0, "/bin/kill", "-CONT", number);
	CHECK(waitpid(stopped, &status, 0) == stopped && WIFEXITED(status));

	// pgrep finds the sleeper among this process's children, pkill ends
	// it with SIGTERM, and kill -0 then finds nothing.
	pid_t sleeper = start_sleeper();
	text_of(sleeper, number);
	char expected[32];
	snprintf(expected, sizeof(expected), "%d\n", sleeper);
	OUTPUT("", expected, "/bin/pgrep", "-P", me, "-x", "sleep");
	STATUS(0, "/bin/kill", "-0", number);
	STATUS(0, "/bin/pkill", "-P", me, "-x", "sleep");
	CHECK(waitpid(sleeper, &status, 0) == sleeper && WIFSIGNALED(status) &&
			WTERMSIG(status) == SIGTERM);
	STATUS(1, "/bin/kill", "-0", number);
	STATUS(1, "/bin/pgrep", "-P", me, "-x", "sleep");
	STATUS(2, "/bin/pgrep", "--no-such-option");
	OUTPUT("", "TERM\n", "/bin/kill", "-l", "15");
	OUTPUT("", "9\n", "/bin/kill", "-l", "KILL");
	sleeper = start_sleeper();
	STATUS(0, "/bin/kill", "-s", "KILL", text_of(sleeper, number));
	CHECK(waitpid(sleeper, &status, 0) == sleeper && WIFSIGNALED(status) &&
			WTERMSIG(status) == SIGKILL);

	// A child that ended and is not waited for yet is a zombie, which
	// pgrep finds, as procps's does, and -a shows as ps shows it.
	pid_t zombie = fork();
	if(!zombie)
		_exit(0);
	text_of(zombie, number);
	for(int i = 0; i < 500; i++) {
		CHECK(run_program((const char *const[]){"/bin/ps", "-o", "stat=", "-p", number, NULL},
					  "", 0) == 0);
		if(output[0] == 'Z')
			break;
		nanosleep(&(struct timespec){.tv_nsec = 10000000}, NULL);
	}
	CHECK(output[0] == 'Z');
	snprintf(expected, sizeof(expected), "%d\n", zombie);
	OUTPUT("", expected, "/bin/pgrep", "-P", me, "-r", "Z");
	snprintf(expected, sizeof(expected), "%d [admin] <defunct>\n", zombie);
	OUTPUT("", expected, "/bin/pgrep", "-a", "-P", me);
	CHECK(waitpid(zombie, &status, 0) == zombie && WIFEXITED(status));
}

// who, w, users and uptime: the runner's session on its terminal.
static void sessions(void) {
	OUTPUT("", "root\n", "/bin/users");
	CONTAINS("root     pts/", "/bin/who");
	CONTAINS("load average:", "/bin/w");
	CONTAINS("load average:", "/bin/uptime");
	CHECK(run_program((const char *const[]){"/bin/uptime", "-p", NULL}, "", 0) == 0);
	CHECK(!strncmp(output, "up ", 3));
}

// free, vmstat and df.
static void memory(void) {
	CONTAINS("Mem:", "/bin/free");
	CONTAINS("Swap:", "/bin/free", "-m");
	CONTAINS("buff/cache", "/bin/free", "-h");
	CONTAINS("procs", "/bin/vmstat");
	CONTAINS(" /tmp\n", "/bin/df", "-P", "/tmp");
	CONTAINS("tmpfs", "/bin/df", "-T", "/tmp");
	STATUS(1, "/bin/df", "/nowhere");
}

// renice, ionice and taskset: levels, classes and CPUs, set on a child
// and inherited through fork and exec.
static void priorities(void) {
	char number[24];
	pid_t sleeper = start_sleeper();
	text_of(sleeper, number);
	CONTAINS("old priority 0, new priority 5", "/bin/renice", "-n", "5", "-p", number);
	errno = 0;
	CHECK(getpriority(PRIO_PROCESS, sleeper) == 5 && errno == 0);
	CONTAINS("old priority 5, new priority 7", "/bin/renice", "--relative", "2", "-p", number);
	CHECK(run_program((const char *const[]){"/bin/ps", "-o", "ni=", "-p", number, NULL}, "", 0) ==
			0);
	CHECK(atoi(output) == 7);

	OUTPUT("", "none: prio 0\n", "/bin/ionice", "-p", number);
	STATUS(0, "/bin/ionice", "-c", "idle", "-p", number);
	OUTPUT("", "idle\n", "/bin/ionice", "-p", number);
	char path[64], text[4096];
	snprintf(path, sizeof(path), "/processes/%d/status", sleeper);
	CHECK(strstr(read_file(path, text, sizeof(text)), "\nio_class: idle\n") != NULL);
	end_sleeper(sleeper);
	OUTPUT("", "best-effort: prio 3\n", "/bin/ionice", "-c", "2", "-n", "3", "/bin/ionice");
	OUTPUT("", "idle\n", "/bin/ionice", "-c", "3", "/bin/sh", "-c", "/bin/ionice");
	STATUS(1, "/bin/ionice", "-c", "bogus", "-p", "0");

	char me[24], expected[64];
	text_of(getpid(), me);
	snprintf(expected, sizeof(expected), "pid %d's current affinity mask: f\n", getpid());
	OUTPUT("", expected, "/bin/taskset", "-p", me);
	snprintf(expected, sizeof(expected), "pid %d's current affinity list: 0-3\n", getpid());
	OUTPUT("", expected, "/bin/taskset", "-cp", me);
	OUTPUT("", "pid 0's current affinity list: 2\n", "/bin/taskset", "-c", "2", "/bin/taskset",
			"-cp", "0");
	OUTPUT("", "pid 0's current affinity list: 1,3\n", "/bin/taskset", "0xa", "/bin/sh", "-c",
			"/bin/taskset -cp 0");
	STATUS(1, "/bin/taskset", "-c", "9", "/bin/true");
	STATUS(1, "/bin/taskset", "-c", "x", "/bin/true");
}

// lsof, chmod, chown, chgrp and which.
static void files(void) {
	STATUS(0, "/bin/mkdir", "-p", DIR);
	write_file(DIR "/held", "held\n");
	write_file(DIR "/unheld", "");
	int fd = open(DIR "/held", O_RDONLY | O_CLOEXEC);
	CHECK(fd >= 0);
	char me[24], expected[32];
	text_of(getpid(), me);
	snprintf(expected, sizeof(expected), "%d\n", getpid());
	OUTPUT("", expected, "/bin/lsof", "-t", DIR "/held");
	CONTAINS(DIR "/held", "/bin/lsof", "--files", "-p", me);
	CONTAINS("rtd", "/bin/lsof", "-p", me);
	STATUS(1, "/bin/lsof", DIR "/unheld");
	close(fd);
	STATUS(1, "/bin/lsof", DIR "/held");

	STATUS(0, "/bin/chmod", "644", DIR "/held");
	STATUS(0, "/bin/chmod", "-R", "u+rwX,go-w", DIR);
	STATUS(1, "/bin/chmod", "999", DIR "/held");
	STATUS(1, "/bin/chmod", "644", DIR "/nowhere");
	STATUS(0, "/bin/chown", "root:root", DIR "/held");
	STATUS(1, "/bin/chown", "nosuchuser", DIR "/held");
	STATUS(0, "/bin/chgrp", "root", DIR "/held");

	OUTPUT("", "/system/commands/ps\n", "/bin/which", "ps");
	STATUS(1, "/bin/which", "nosuchcommand");
}

// rtrace: a command's calls, a count of them, a child it forks, and a
// running process it attaches to.
static void tracing(void) {
	char text[16384];
	OUTPUT("", "held\n", "/bin/rtrace", "-o", DIR "/trace", "/bin/cat", DIR "/held");
	read_file(DIR "/trace", text, sizeof(text));
	CHECK(strstr(text, "directory.open(") != NULL);
	CHECK(strstr(text, "admin/held") != NULL);
	CHECK(strstr(text, "file.read(") != NULL);
	CHECK(strstr(text, "\"held\\n\"") != NULL);
	CHECK(strstr(text, "+++ exited with 0 +++") != NULL);
	OUTPUT("", "held\n", "/bin/rtrace", "-e", "trace=open", "-o", DIR "/trace", "/bin/cat",
			DIR "/held");
	read_file(DIR "/trace", text, sizeof(text));
	CHECK(strstr(text, "directory.open(") != NULL && strstr(text, "file.read(") == NULL);
	STATUS(1, "/bin/rtrace", "-o", "/devices/null", "/bin/false");
	STATUS(0, "/bin/rtrace", "-c", "-o", DIR "/count", "/bin/true");
	CHECK(strstr(read_file(DIR "/count", text, sizeof(text)), "% time") != NULL);
	STATUS(0, "/bin/rtrace", "-f", "-o", DIR "/forks", "/bin/sh", "-c", "/bin/true; /bin/true");
	read_file(DIR "/forks", text, sizeof(text));
	CHECK(strstr(text, "--- child ") != NULL && strstr(text, "[pid ") != NULL);

	// A process already running: it goes on while traced, and its end
	// ends the trace.
	pid_t busy = fork();
	if(!busy) {
		for(int i = 0; i < 40; i++) {
			getppid();
			check_sleep_ms(25);
		}
		_exit(0);
	}
	char number[24];
	STATUS(0, "/bin/rtrace", "-o", DIR "/attached", "-p", text_of(busy, number));
	int status;
	CHECK(waitpid(busy, &status, 0) == busy && WIFEXITED(status) && WEXITSTATUS(status) == 0);
	CHECK(strstr(read_file(DIR "/attached", text, sizeof(text)), "+++ exited with 0 +++") !=
			NULL);

	// As strace: a command is found before anything starts, results line
	// up at column 40, -r's times are six wide, and a command a signal
	// ends ends rtrace by the same signal.
	RUN_WITH(1, "", "rtrace: Cannot find executable 'nosuchcommand'\n", 0, "/bin/sh", "-c",
			"/bin/rtrace nosuchcommand 2>&1");
	RUN_WITH(1, "", "rtrace: Cannot stat '" DIR "/missing': No such file or directory\n", 0,
			"/bin/sh", "-c", "/bin/rtrace " DIR "/missing 2>&1");
	STATUS(0, "/bin/rtrace", "-r", "-o", DIR "/relative", "/bin/true");
	read_file(DIR "/relative", text, sizeof(text));
	CHECK(!strncmp(text, "     0.000000 ", 14));
	const char *equals = strstr(text, " = ");
	const char *start = equals;
	while(start > text && start[-1] != '\n')
		start--;
	CHECK(equals && (equals - start == 39 || equals[-1] != ' '));
	pid_t tracer = fork();
	if(!tracer) {
		execl("/bin/rtrace", "rtrace", "-o", "/devices/null", "/bin/sh", "-c", "kill -TERM $$",
				(char *)NULL);
		_exit(127);
	}
	CHECK(waitpid(tracer, &status, 0) == tracer && WIFSIGNALED(status) &&
			WTERMSIG(status) == SIGTERM);
}

// dmesg and /devices/kmsg: a line written to the log, the records as they
// are read, and the log cleared. The log keeps the latest 512 records,
// so the boot's own may be gone by now.
static void kernel_log(void) {
	int fd = open("/devices/kmsg", O_RDWR | O_CLOEXEC);
	CHECK(fd >= 0);
	static const char LINE[] = "<4>admin: a line for dmesg\n";
	CHECK(write(fd, LINE, sizeof(LINE) - 1) == sizeof(LINE) - 1);
	// A read is one record: `priority,sequence,microseconds,-;text`.
	char record[512];
	ssize_t got = read(fd, record, sizeof(record) - 1);
	CHECK(got > 0);
	record[got > 0 ? got : 0] = 0;
	unsigned priority;
	unsigned long long sequence, micros;
	CHECK(sscanf(record, "%u,%llu,%llu,-;", &priority, &sequence, &micros) == 3);
	CHECK(strchr(record, '\n') == record + strlen(record) - 1);
	close(fd);
	// The whole log is more than `output` holds.
	CONTAINS("] admin: admin: a line for dmesg\n", "/bin/sh", "-c", "/bin/dmesg | /bin/tail -n 3");
	CONTAINS("a line for dmesg", "/bin/dmesg", "-l", "warn");
	CHECK(run_program((const char *const[]){"/bin/dmesg", "-l", "err", NULL}, "", 0) == 0);
	CHECK(strstr(output, "a line for dmesg") == NULL);
	CONTAINS("<4>", "/bin/dmesg", "-r", "-l", "warn");
	CONTAINS("kern  :warn  : ", "/bin/dmesg", "-x", "-l", "warn");
	STATUS(0, "/bin/dmesg", "-C");
	CHECK(run_program((const char *const[]){"/bin/dmesg", NULL}, "", 0) == 0);
	CHECK(strstr(output, "a line for dmesg") == NULL);
	STATUS(1, "/bin/dmesg", "-n", "1");
}

// xxd, hexdump, hd, column and tree, on what util-linux's, Vim's and
// tree's own write for the same input. tree's line down is followed by
// no-break spaces.
static void dumping(void) {
	OUTPUT("hello, world\n", "00000000: 6865 6c6c 6f2c 2077 6f72 6c64 0a         hello, world.\n",
			"/bin/xxd");
	OUTPUT("hi\n", "68690a\n", "/bin/xxd", "-p");
	OUTPUT("68690a\n", "hi\n", "/bin/xxd", "-r", "-p");
	OUTPUT("hi", "  0x68, 0x69\n", "/bin/xxd", "-i");
	STATUS(2, "/bin/xxd", DIR "/nowhere");
	OUTPUT("hello, world\n",
			"00000000  68 65 6c 6c 6f 2c 20 77  6f 72 6c 64 0a           |hello, world.|\n"
			"0000000d\n",
			"/bin/hd");
	OUTPUT("hello, world\n", "0000000 6568 6c6c 2c6f 7720 726f 646c 000a     \n000000d\n",
			"/bin/hexdump");
	OUTPUT("hello, world\n",
			"0000000  104 101 108 108 111  44  32 119\n0000008  111 114 108 100  10            \n",
			"/bin/hexdump", "-e", "\"%07.7_ad \" 8/1 \"%4u\" \"\\n\"");
	OUTPUT("a b c\nlong x y\n", "a     b  c\nlong  x  y\n", "/bin/column", "-t");
	OUTPUT("a b\n1 \n",
			"{\n   \"table\": [\n      {\n         \"a\": \"a\",\n         \"b\": \"b\"\n"
			"      },{\n         \"a\": \"1\",\n         \"b\": null\n      }\n   ]\n}\n",
			"/bin/column", "-t", "-N", "A,B", "-J");
	STATUS(0, "/bin/mkdir", "-p", DIR "/t/a/b", DIR "/t/c");
	STATUS(0, "/bin/touch", DIR "/t/a/f", DIR "/t/c/g");
	OUTPUT("",
			DIR "/t\n├── a\n│\u00a0\u00a0 ├── b\n│\u00a0\u00a0 └── f\n└── c\n    └── g\n\n"
			    "4 directories, 2 files\n",
			"/bin/tree", DIR "/t");
	OUTPUT("", DIR "/t\n├── a\n│\u00a0\u00a0 └── b\n└── c\n", "/bin/tree", "-d", "--noreport",
			DIR "/t");
	STATUS(2, "/bin/tree", DIR "/nowhere");
}

// script, watch and pidstat, the tools that run things over time.
static void watching(void) {
	OUTPUT("", "hi\r\n", "/bin/script", "-q", "-c", "echo hi", DIR "/typescript");
	char text[4096];
	read_file(DIR "/typescript", text, sizeof(text));
	CHECK(!strncmp(text, "Script started on ", 18));
	CHECK(strstr(text, "COMMAND=\"echo hi\"") != NULL);
	CHECK(strstr(text, "\nhi\r\n") != NULL);
	CHECK(strstr(text, "[COMMAND_EXIT_CODE=\"0\"]\n") != NULL);
	STATUS(3, "/bin/script", "-q", "-e", "-c", "exit 3", DIR "/typescript");
	STATUS(0, "/bin/script", "-q", "-c", "exit 3", DIR "/typescript");

	// watch -g ends when what it shows changes: here once it has shown
	// the file as it was, which its command notes.
	write_file(DIR "/flag", "before\n");
	pid_t changer = fork();
	if(!changer) {
		long long deadline = check_now_ns() + 30000000000LL;
		while(access(DIR "/seen", F_OK) != 0 && check_now_ns() < deadline)
			check_sleep_ms(10);
		write_file(DIR "/flag", "changed\n");
		_exit(0);
	}
	CONTAINS("changed", "/bin/watch", "-g", "-t", "-n", "0.1",
			"/bin/cat " DIR "/flag; /bin/touch " DIR "/seen");
	waitpid(changer, NULL, 0);
	STATUS(8, "/bin/watch", "-e", "-t", "-n", "0.1", "/bin/false");

	char me[24];
	text_of(getpid(), me);
	CONTAINS("%usr %system", "/bin/pidstat", "-p", me);
	CONTAINS("  admin\n", "/bin/pidstat", "-p", me);
	CONTAINS("kB_rd/s", "/bin/pidstat", "-d", "-p", me);
	CONTAINS("TGID", "/bin/pidstat", "-t", "-p", me);
	CONTAINS("Average:", "/bin/pidstat", "-p", me, "1", "1");
	CONTAINS("  /system/tests/posix/admin\n", "/bin/pidstat", "-l", "-p", me);
	CONTAINS("    USER", "/bin/pidstat", "-U", "-p", me);
	CONTAINS("    root ", "/bin/pidstat", "-U", "root", "-p", me);
	CONTAINS("  admin\n", "/bin/pidstat", "-G", "^adm", "-p", "ALL");
	CONTAINS(" threads   fd-nr  Command\n", "/bin/pidstat", "-v", "-p", me);
	CONTAINS("    0 NORMAL  admin\n", "/bin/pidstat", "-R", "-p", me);
	// Users and names that none of them have.
	CHECK(run_program((const char *const[]){"/bin/pidstat", "-U", "nosuchuser", "-p", me, NULL},
				  "", 0) == 0);
	CHECK(strstr(output, "admin") == NULL);
	CHECK(run_program((const char *const[]){"/bin/pidstat", "-G", "^nosuchname$", "-p", "ALL",
						  NULL},
				  "", 0) == 0);
	CHECK(strstr(output, "  Command\n") != NULL && strstr(output, "  Command\n")[10] == 0);
	// -H: the time as seconds since the epoch, in the heading too.
	CHECK(run_program((const char *const[]){"/bin/pidstat", "-H", "-p", me, NULL}, "", 0) == 0);
	const char *heading = strstr(output, "\n\n");
	char *end = NULL;
	CHECK(heading && strtoll(heading + 2, &end, 10) > 1000000000LL && *end == ' ');
	// One process in one report: one heading, the averages under it.
	CHECK(run_program((const char *const[]){"/bin/pidstat", "-p", me, "1", "2", NULL}, "", 0) ==
			0);
	CHECK(strstr(output, "%usr") && !strstr(strstr(output, "%usr") + 1, "%usr"));
	CHECK(strstr(output, "\nAverage:    ") != NULL);

	// Interrupted, it prints the averages it has, as sysstat's does.
	int fds[2];
	CHECK(pipe(fds) == 0);
	pid_t watcher = fork();
	if(!watcher) {
		dup2(fds[1], 1);
		close(fds[0]);
		close(fds[1]);
		execl("/bin/pidstat", "pidstat", "-p", me, "1", (char *)NULL);
		_exit(127);
	}
	close(fds[1]);
	// After a report or two.
	check_sleep_ms(2500);
	kill(watcher, SIGINT);
	char got[4096];
	size_t len = 0;
	ssize_t n;
	while(len < sizeof(got) - 1 && (n = read(fds[0], got + len, sizeof(got) - 1 - len)) > 0)
		len += n;
	got[len] = 0;
	close(fds[0]);
	int status;
	CHECK(waitpid(watcher, &status, 0) == watcher && WIFEXITED(status) &&
			WEXITSTATUS(status) == 0);
	CHECK(strstr(got, "  admin\n\nAverage:    ") != NULL);
}

// shutdown, reboot, poweroff, halt and service, through Spark's control
// socket (spark(8)): shutdowns planned and called off, and what Spark
// answers in a boot that starts no services; and the watchdog it pets. The `consoles` boot test
// powers the machine off, and `spark-oom` has Spark restart a service.
static void power(void) {
	STATUS(1, "/bin/shutdown", "-c");
	STATUS(0, "/bin/shutdown", "-r", "+30", "a planned restart");
	STATUS(0, "/bin/shutdown", "-c");
	STATUS(1, "/bin/shutdown", "-c");
	STATUS(0, "/bin/shutdown", "-H", "--no-wall", "23:59");
	STATUS(0, "/bin/shutdown", "--no-wall", "-c");
	STATUS(0, "/bin/shutdown", "-k", "now");
	STATUS(1, "/bin/shutdown", "tomorrow");
	CONTAINS("Reboot the system.", "/bin/reboot", "--help");
	CONTAINS("Power off the system.", "/bin/poweroff", "--help");
	CONTAINS("Halt the system.", "/bin/halt", "--help");
	STATUS(1, "/bin/halt", "--no-such-option");
	OUTPUT("", "", "/bin/service", "--status-all");
	STATUS(4, "/bin/service", "nothing", "status");
	STATUS(1, "/bin/service", "nothing", "start");
	STATUS(1, "/bin/service");
	// q35's TCO timer, which Spark starts only once it supervises: after
	// the boot program the tests run in (spark(8)'s WATCHDOG).
	CONTAINS("Device:    the Intel ICH9 TCO timer\nState:     stopped\n", "/bin/watchdog");
	STATUS(1, "/bin/watchdog", "now");
}

// The program behind the links.
static void links(void) {
	CONTAINS("\nxxd\n", "/system/commands/systools", "--list");
	OUTPUT("", "spark\n", "/system/commands/systools", "ps", "-o", "comm=", "-p", "1");
	struct stat st;
	CHECK(lstat("/system/commands/ps", &st) == 0 && S_ISLNK(st.st_mode));
	STATUS(0, "/bin/rm", "-r", DIR);
}

int main(void) {
	check_start("admin");
	RUN(proc);
	RUN(processes);
	RUN(sessions);
	RUN(memory);
	RUN(priorities);
	RUN(files);
	RUN(tracing);
	RUN(kernel_log);
	RUN(dumping);
	RUN(watching);
	RUN(power);
	RUN(links);
	return check_end();
}
