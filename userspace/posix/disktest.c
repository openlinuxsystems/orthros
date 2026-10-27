// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// disktest: the disk check of a real machine (ADR 0122). The same program
// runs on Dukovo and on Linux, on one disk, which it finds by its serial
// number rather than by its name, since names follow the order the
// controllers are found in. Everything on that disk is lost, so it does
// nothing without --yes-erase.
//
// On Dukovo, `disktest --yes-erase SERIAL` writes a pattern to 64 places
// spread over the disk, from its first MiB to its last, and one write off
// a sector's boundary; empties the disk's write cache; reads every place
// back; measures 4 KiB, 64 KiB and 1 MiB reads and writes one after the
// other, as `ob bench` does; and saves what it found in the disk's report
// area, for Linux.
//
// On Linux, from a live system after a restart, the same command reads
// Dukovo's report, checks that Linux reads Dukovo's pattern back from
// every place, measures the same transfers, and prints the comparison in
// Markdown. `--device PATH` names the disk, or a disk image file, instead
// of a serial number, for the boot test.
//
// The disk's layout: the first MiB is a pattern place, the next 32 MiB
// the transfers' span, the MiB after them the report area, and the other
// 63 places spread from there to the disk's last MiB.

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#ifdef __radix__
#define BLOCK_DIR "/kernel/block"
#define DEVICE_DIR "/devices"
#define SYSTEM "Dukovo"
#else
#define BLOCK_DIR "/sys/block"
#define DEVICE_DIR "/dev"
#define SYSTEM "Linux"
#endif

#define MIB (1u << 20)
// Where the transfers go, and how far.
#define SPAN_AT ((off_t)MIB)
#define SPAN (32u << 20)
// Where the report is.
#define REPORT_AT ((off_t)33 * MIB)
// Where the places after the first start.
#define PLACES_AT ((off_t)34 * MIB)
#define PLACES 64
// The smallest disk the layout fits, with a MiB between places.
#define SMALLEST ((off_t)128 * MIB)
// How many times each transfer is measured; the fastest counts, as in
// `ob bench`.
#define REPEATS 3
// The first line of a report, which says it is one.
#define MAGIC "Dukovo disk check report 1\n"

// What a run found.
struct results {
	uint64_t run;
	int written, read_back, small;
	uint64_t ns[6];
};

// The transfers, as `ob bench` measures them (userspace/posix/bench.c).
static const struct {
	const char *key, *name;
	int writing;
	size_t size;
	unsigned rounds;
} TRANSFERS[6] = {
		{"read_4k", "4 KiB read", 0, 4096, 20000},
		{"write_4k", "4 KiB write", 1, 4096, 20000},
		{"read_64k", "64 KiB read", 0, 64 * 1024, 5000},
		{"write_64k", "64 KiB write", 1, 64 * 1024, 5000},
		{"read_mib", "1 MiB read", 0, MIB, 500},
		{"write_mib", "1 MiB write", 1, MIB, 500},
};

static uint64_t now_ns(void) {
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (uint64_t)ts.tv_sec * 1000000000u + (uint64_t)ts.tv_nsec;
}

static void die(const char *what) {
	printf("disktest: FAILED: %s: %s\n", what, strerror(errno));
	exit(1);
}

// A well mixed 64-bit word of `x` (the SplitMix64 finaliser).
static uint64_t mix(uint64_t x) {
	x += 0x9e3779b97f4a7c15u;
	x = (x ^ (x >> 30)) * 0xbf58476d1ce4e5b9u;
	x = (x ^ (x >> 27)) * 0x94d049bb133111ebu;
	return x ^ (x >> 31);
}

// The pattern of run `run` for the MiB at `at`: different at every
// offset of the disk and in every run.
static void pattern(uint64_t *words, off_t at, uint64_t run) {
	for(size_t i = 0; i < MIB / 8; i++)
		words[i] = mix(((uint64_t)at / 8 + i) ^ run);
}

// Where place `i` of a disk of `size` bytes starts.
static off_t place(int i, off_t size) {
	if(i == 0)
		return 0;
	if(i == PLACES - 1)
		return size - MIB;
	off_t step = (size - MIB - PLACES_AT) / (PLACES - 2);
	off_t at = PLACES_AT + step * (i - 1);
	return at - at % 4096;
}

// The text of the file at `path` into `out`, without the white space and
// NULs around it; empty if there is none.
static void read_text(const char *path, char *out, size_t size) {
	out[0] = 0;
	FILE *file = fopen(path, "r");
	if(!file)
		return;
	size_t n = fread(out, 1, size - 1, file);
	fclose(file);
	out[n] = 0;
	// A SATA disk's serial number on Linux is in SCSI's unit serial
	// number page, after its four bytes of header.
	if(n > 4 && strstr(path, "vpd_pg80")) {
		size_t length = (unsigned char)out[3];
		if(length > n - 4)
			length = n - 4;
		memmove(out, out + 4, length);
		out[length] = 0;
		n = length;
	}
	while(n > 0 && (out[n - 1] == ' ' || out[n - 1] == '\n' || out[n - 1] == 0))
		out[--n] = 0;
	size_t skip = strspn(out, " ");
	memmove(out, out + skip, n - skip + 1);
}

// A disk's attribute in BLOCK_DIR, as text.
static void attribute(const char *name, const char *file, char *out, size_t size) {
	char path[512];
	snprintf(path, sizeof(path), BLOCK_DIR "/%s/%s", name, file);
	read_text(path, out, size);
}

// The serial number of disk `name`, or empty.
static void serial_of(const char *name, char *out, size_t size) {
	attribute(name, "device/serial", out, size);
#ifndef __radix__
	if(!out[0])
		attribute(name, "device/vpd_pg80", out, size);
#endif
}

// Finds the one disk whose serial number is `serial`; says which there
// are when none is, or more than one.
static void find_disk(const char *serial, char *name, size_t size) {
	DIR *dir = opendir(BLOCK_DIR);
	if(!dir)
		die(BLOCK_DIR);
	int found = 0;
	struct dirent *entry;
	char seen[2048] = "";
	while((entry = readdir(dir))) {
		if(entry->d_name[0] == '.')
			continue;
		char got[128];
		serial_of(entry->d_name, got, sizeof(got));
		if(!got[0])
			continue;
		size_t used = strlen(seen);
		snprintf(seen + used, sizeof(seen) - used, "\n  %s: %s", entry->d_name, got);
		if(!strcmp(got, serial)) {
			found++;
			snprintf(name, size, "%s", entry->d_name);
		}
	}
	closedir(dir);
	if(found != 1) {
		printf("disktest: FAILED: %s disk has the serial number %s; the disks are:%s\n",
		       found ? "more than one" : "no", serial, seen[0] ? seen : " none with one");
		exit(1);
	}
}

static void transfer(int fd, void *buffer, size_t size, off_t at, int writing, const char *what) {
	ssize_t done = writing ? pwrite(fd, buffer, size, at) : pread(fd, buffer, size, at);
	if(done != (ssize_t)size) {
		printf("disktest: FAILED: %s of %zu bytes at %lld: %s\n", what, size, (long long)at,
		       done < 0 ? strerror(errno) : "short");
		exit(1);
	}
}

// How many of the disk's places hold run `run`'s pattern, naming the
// first that does not.
static int check_places(int fd, off_t size, uint64_t *buffer, uint64_t *want, uint64_t run) {
	int good = 0, told = 0;
	for(int i = 0; i < PLACES; i++) {
		off_t at = place(i, size);
		transfer(fd, buffer, MIB, at, 0, "a read");
		pattern(want, at, run);
		if(!memcmp(buffer, want, MIB)) {
			good++;
			continue;
		}
		size_t word = 0;
		while(buffer[word] == want[word])
			word++;
		if(!told++)
			printf("disktest: place %d at %lld differs from byte %zu on\n", i, (long long)at,
			       word * 8);
	}
	return good;
}

// The transfers, each measured REPEATS times; their fastest times per
// transfer go in `ns`.
static void measure(int fd, uint64_t *buffer, uint64_t ns[6]) {
	memset(buffer, 0x5a, MIB);
	for(int t = 0; t < 6; t++) {
		ns[t] = UINT64_MAX;
		for(int r = 0; r < REPEATS; r++) {
			off_t at = 0;
			uint64_t started = now_ns();
			for(unsigned i = 0; i < TRANSFERS[t].rounds; i++) {
				transfer(fd, buffer, TRANSFERS[t].size, SPAN_AT + at, TRANSFERS[t].writing,
				         TRANSFERS[t].writing ? "a write" : "a read");
				at = (at + (off_t)TRANSFERS[t].size) % SPAN;
			}
			uint64_t each = (now_ns() - started) / TRANSFERS[t].rounds;
			if(each < ns[t])
				ns[t] = each;
		}
		printf("disktest: %s: %llu ns\n", TRANSFERS[t].name, (unsigned long long)ns[t]);
	}
}

#ifdef __radix__
// A write of 3 bytes off a sector's boundary, in the report area before
// the report: those bytes change, and the rest of the sector stays.
static int small_write(int fd, uint64_t *buffer) {
	unsigned char *bytes = (unsigned char *)buffer;
	transfer(fd, bytes, 4096, REPORT_AT, 0, "a read");
	unsigned char before[4096];
	memcpy(before, bytes, sizeof(before));
	unsigned char three[3] = {(unsigned char)~before[513], (unsigned char)~before[514],
	                          (unsigned char)~before[515]};
	transfer(fd, three, 3, REPORT_AT + 513, 1, "a write");
	transfer(fd, bytes, 4096, REPORT_AT, 0, "a read");
	memcpy(before + 513, three, 3);
	return !memcmp(bytes, before, sizeof(before));
}

// Dukovo's side: write, read back, measure, and save the report.
static int dukovo(int fd, off_t size, uint64_t *buffer, uint64_t *want) {
	struct results r = {0};
	r.run = mix(now_ns() ^ (uint64_t)time(NULL));
	r.small = small_write(fd, buffer);
	printf("disktest: a write of 3 bytes off a sector's boundary: %s\n",
	       r.small ? "passed" : "FAILED");
	for(int i = 0; i < PLACES; i++) {
		off_t at = place(i, size);
		pattern(buffer, at, r.run);
		transfer(fd, buffer, MIB, at, 1, "a write");
	}
	r.written = PLACES;
	if(fsync(fd) != 0)
		die("emptying the disk's write cache");
	r.read_back = check_places(fd, size, buffer, want, r.run);
	printf("disktest: %d of %d places read back as written\n", r.read_back, PLACES);
	measure(fd, buffer, r.ns);
	char *text = (char *)buffer;
	memset(text, 0, MIB);
	int n = snprintf(text, MIB, MAGIC "run %016llx\nwritten %d\nread_back %d\nsmall %d\n",
	                 (unsigned long long)r.run, r.written, r.read_back, r.small);
	for(int t = 0; t < 6; t++)
		n += snprintf(text + n, MIB - n, "%s %llu\n", TRANSFERS[t].key,
		              (unsigned long long)r.ns[t]);
	transfer(fd, text, MIB, REPORT_AT, 1, "writing the report");
	if(fsync(fd) != 0)
		die("emptying the disk's write cache");
	int passed = r.small && r.read_back == PLACES;
	printf("disktest: %s; the report is saved on the disk. Restart into a Linux live system "
	       "and run the stick's crosscheck/run.sh for the comparison\n",
	       passed ? "passed" : "FAILED");
	return passed ? 0 : 1;
}
#else
// The number after `key` and a space on `line`, in base `base`; false if
// the line is not that key's. (Not sscanf, which glibc 2.38 and later
// give a new version of, which older live systems lack.)
static int field(const char *line, const char *key, unsigned base, unsigned long long *value) {
	size_t n = strlen(key);
	if(strncmp(line, key, n) || line[n] != ' ' || !line[n + 1])
		return 0;
	unsigned long long got = 0;
	for(const char *c = line + n + 1; *c; c++) {
		unsigned digit = *c >= '0' && *c <= '9'   ? (unsigned)(*c - '0')
		                 : *c >= 'a' && *c <= 'f' ? (unsigned)(*c - 'a' + 10)
		                                          : base;
		if(digit >= base)
			return 0;
		got = got * base + digit;
	}
	*value = got;
	return 1;
}

// Dukovo's report on the disk, if there is one.
static int read_report(int fd, char *text, struct results *r) {
	transfer(fd, text, MIB, REPORT_AT, 0, "reading the report");
	text[MIB - 1] = 0;
	if(strncmp(text, MAGIC, strlen(MAGIC)))
		return 0;
	memset(r, 0, sizeof(*r));
	int fields = 0;
	for(char *line = text + strlen(MAGIC); *line;) {
		char *end = strchr(line, '\n');
		if(!end)
			break;
		*end = 0;
		unsigned long long value;
		if(field(line, "run", 16, &value)) {
			r->run = value;
			fields++;
		} else if(field(line, "written", 10, &value)) {
			r->written = (int)value;
			fields++;
		} else if(field(line, "read_back", 10, &value)) {
			r->read_back = (int)value;
			fields++;
		} else if(field(line, "small", 10, &value)) {
			r->small = (int)value;
			fields++;
		}
		for(int t = 0; t < 6; t++)
			if(field(line, TRANSFERS[t].key, 10, &value)) {
				r->ns[t] = value;
				fields++;
			}
		line = end + 1;
	}
	return fields == 10;
}

// A time in microseconds, for the table.
static void micros(char *out, size_t size, uint64_t ns) {
	snprintf(out, size, "%llu.%01llu µs", (unsigned long long)(ns / 1000),
	         (unsigned long long)(ns % 1000 / 100));
}

// Linux's side: read Dukovo's report and pattern back, measure, compare.
static int linux_side(int fd, off_t size, uint64_t *buffer, uint64_t *want, const char *model,
                      const char *serial) {
	struct results dukovo;
	if(!read_report(fd, (char *)buffer, &dukovo)) {
		printf("disktest: FAILED: the disk holds no report of Dukovo's: boot the Dukovo stick's "
		       "disk check first\n");
		return 1;
	}
	int linux_reads = check_places(fd, size, buffer, want, dukovo.run);
	uint64_t ns[6];
	measure(fd, buffer, ns);
	printf("\n# Disk check: %s, serial %s\n\n", model, serial);
	printf("| Check | Result |\n| --- | --- |\n");
	printf("| Dukovo wrote %d places and read them back | %d of %d |\n", dukovo.written,
	       dukovo.read_back, dukovo.written);
	printf("| Dukovo's write of 3 bytes off a sector's boundary | %s |\n",
	       dukovo.small ? "passed" : "FAILED");
	printf("| Linux reads what Dukovo wrote, after a restart | %d of %d |\n\n", linux_reads,
	       PLACES);
	printf("| Transfer, one after the other | Dukovo | Linux | Dukovo / Linux |\n");
	printf("| --- | ---: | ---: | ---: |\n");
	int slower = 0;
	for(int t = 0; t < 6; t++) {
		char a[32], b[32];
		micros(a, sizeof(a), dukovo.ns[t]);
		micros(b, sizeof(b), ns[t]);
		double ratio = ns[t] ? (double)dukovo.ns[t] / (double)ns[t] : 0;
		slower += ratio > 1.0;
		printf("| %s | %s | %s | %.2f |\n", TRANSFERS[t].name, a, b, ratio);
	}
	int passed = dukovo.small && dukovo.read_back == dukovo.written && linux_reads == PLACES;
	printf("\nThe checks %s. %s\n", passed ? "passed" : "FAILED",
	       slower ? "Some transfers are slower on Dukovo than on Linux."
	              : "Every transfer is as fast on Dukovo as on Linux, or faster.");
	return passed ? 0 : 1;
}
#endif

int main(int argc, char **argv) {
	const char *serial = NULL, *device = NULL;
	int erase = 0;
	for(int i = 1; i < argc; i++) {
		if(!strcmp(argv[i], "--yes-erase"))
			erase = 1;
		else if(!strcmp(argv[i], "--device") && i + 1 < argc)
			device = argv[++i];
		else
			serial = argv[i];
	}
	if(!erase || (!serial && !device)) {
		printf("usage: disktest --yes-erase SERIAL, or --yes-erase --device PATH\n"
		       "Everything on the disk is lost.\n");
		return 2;
	}
	char name[256] = "", path[512], model[128] = "", found_serial[128] = "";
	if(device) {
		snprintf(path, sizeof(path), "%s", device);
		snprintf(model, sizeof(model), "%s", device);
		snprintf(found_serial, sizeof(found_serial), "%s", serial ? serial : "none");
	} else {
		find_disk(serial, name, sizeof(name));
		snprintf(path, sizeof(path), DEVICE_DIR "/%s", name);
		attribute(name, "device/model", model, sizeof(model));
		snprintf(found_serial, sizeof(found_serial), "%s", serial);
	}
	// Linux's disk tools open a disk so to go around its page cache; a
	// file on a filesystem without it is opened plainly.
	int fd = open(path, O_RDWR | O_DIRECT);
	if(fd < 0 && errno == EINVAL && device)
		fd = open(path, O_RDWR);
	if(fd < 0)
		die(path);
	off_t size = lseek(fd, 0, SEEK_END);
	if(size < SMALLEST) {
		printf("disktest: FAILED: %s holds %lld bytes, fewer than %lld\n", path, (long long)size,
		       (long long)SMALLEST);
		return 1;
	}
	char logical[32] = "", physical[32] = "", rotational[8] = "";
	if(name[0]) {
		attribute(name, "queue/logical_block_size", logical, sizeof(logical));
		attribute(name, "queue/physical_block_size", physical, sizeof(physical));
		attribute(name, "queue/rotational", rotational, sizeof(rotational));
	}
	printf("disktest: %s on %s: %s, serial %s, %lld bytes, %s-byte sectors, %s-byte blocks%s\n",
	       path, SYSTEM, model, found_serial, (long long)size, logical[0] ? logical : "?",
	       physical[0] ? physical : "?",
	       !strcmp(rotational, "1") ? ", a hard disk" : !strcmp(rotational, "0") ? ", solid state" : "");
	uint64_t *buffer = NULL, *want = NULL;
	if(posix_memalign((void **)&buffer, 4096, MIB) || posix_memalign((void **)&want, 4096, MIB))
		die("posix_memalign");
#ifdef __radix__
	int code = dukovo(fd, size, buffer, want);
#else
	int code = linux_side(fd, size, buffer, want, model, found_serial);
#endif
	close(fd);
	return code;
}
