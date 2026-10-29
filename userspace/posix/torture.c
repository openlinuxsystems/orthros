// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// What the btrfs-stress boot test runs: the long filesystem stress run of
// Phase 10. `torture <seed> <seconds> <dir>...` starts two workers in each
// <dir>, each in a directory of its own under <dir>/torture, all at once;
// for <seconds>, each makes random changes there, chosen by <seed> and the
// worker's number: files written whole, in part, past their end and
// appended to, cut and grown, saved through a temporary file and a rename,
// renamed over each other and between three directories, linked and
// removed, symbolic links made and renamed over each other, and a
// directory of small files made and removed, with fsync(2) of files and
// directories between. Half of what is written is text, which compresses,
// and half is not. Each worker keeps in memory what every file should
// hold, reads one back now and then, and checks everything at the end;
// then it syncs it all, and how many changes it made, in w<N>.changes
// beside its directory.
//
// `torture --check <seed> <dir>...`, after a reboot, works out the same
// changes again without touching the disk, as many as each worker made,
// and checks that each worker's directory holds what they leave: every
// file's bytes, size and link count, files linked together as one, every
// symbolic link's target, and nothing else. Then it removes it all, and
// <dir>/torture with it.
//
// It runs on Linux too, to check the test itself: Linux's answers are the
// ones it expects.

#include <dirent.h>
#include <fcntl.h>
#include <stdint.h>
#include <sys/stat.h>

#include "check.h"

// The workers in each directory.
#define WORKERS 2
// The names a worker's files can have, n0 to n15, and so the most files.
#define NAMES 16
// The directories a worker's files are in, under its own.
#define SUBDIRS 3
static const char *const SUBDIR[SUBDIRS] = {"a", "b", "c"};
// The symbolic links a worker keeps, l0 to l3, in its own directory.
#define LINKS 4
// The largest a file gets: 1 MiB.
#define MAX_SIZE (1 << 20)
// The longest symbolic link target.
#define TARGET_MAX 120

// A file, as a worker expects it.
struct file {
	unsigned char *data;
	size_t size;
	// Its names; 0 for an entry not in use.
	int links;
};

// A worker.
struct worker {
	// Its directory, and its number among all the workers.
	char dir[256];
	int number;
	// Whether it changes the disk too, rather than only working out what
	// the changes leave (--check).
	int disk;
	// Its random numbers.
	uint64_t state;
	// The file each name is, as an index into `files`, or -1; and the
	// directory each name is in.
	int names[NAMES];
	int where[NAMES];
	struct file files[NAMES];
	// Each symbolic link's target, or "" for none.
	char links[LINKS][TARGET_MAX + 1];
	// What files are read into.
	unsigned char *buffer;
	// What it has done.
	long changes, synced, read_back;
};

// SplitMix64: the next of a series of random numbers.
static uint64_t mix(uint64_t *state) {
	uint64_t z = (*state += 0x9e3779b97f4a7c15ull);
	z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ull;
	z = (z ^ (z >> 27)) * 0x94d049bb133111ebull;
	return z ^ (z >> 31);
}

// A random number below `n`, which is not 0.
static size_t below(struct worker *w, size_t n) {
	return mix(&w->state) % n;
}

static size_t min_size(size_t a, size_t b) {
	return a < b ? a : b;
}

// What a write with `key` puts at `offset` to `offset + len` of `data`:
// lines of text if `key` is odd, which compress, and otherwise numbers
// that do not. Each byte depends on its place in the file, not on where
// the write began.
static void fill(unsigned char *data, uint64_t key, size_t offset, size_t len) {
	static const char TEXT[] = "the quick brown fox jumps over the lazy dog, again and again\n";
	for(size_t at = offset; at < offset + len; at++) {
		if(key & 1) {
			data[at] = TEXT[(at + key / 2) % (sizeof(TEXT) - 1)];
		} else {
			uint64_t word = at / 8 ^ key;
			data[at] = (unsigned char)(mix(&word) >> (at % 8 * 8));
		}
	}
}

// The path of name `name`.
static void name_path(const struct worker *w, int name, char *path, size_t size) {
	snprintf(path, size, "%s/%s/n%d", w->dir, SUBDIR[w->where[name]], name);
}

// The path of symbolic link `link`, with `suffix` after it.
static void link_path(const struct worker *w, int link, const char *suffix, char *path,
		size_t size) {
	snprintf(path, size, "%s/l%d%s", w->dir, link, suffix);
}

// Says what failed, with errno, and returns -1.
#define FAIL(what) (check_failed(__FILE__, __LINE__, (what), errno), -1)
// Returns -1 from the change if `call` fails.
#define TRY(call) \
	do { \
		if((call) < 0) \
			return FAIL(#call); \
	} while(0)

// Writes all of `data` at `offset` of `fd`.
static int write_at(int fd, const unsigned char *data, size_t len, off_t offset) {
	while(len) {
		ssize_t n = pwrite(fd, data, len, offset);
		if(n <= 0)
			return -1;
		data += n;
		len -= (size_t)n;
		offset += n;
	}
	return 0;
}

// Writes all of `data` at the end of `fd`, which is open to append.
static int write_all(int fd, const unsigned char *data, size_t len) {
	while(len) {
		ssize_t n = write(fd, data, len);
		if(n <= 0)
			return -1;
		data += n;
		len -= (size_t)n;
	}
	return 0;
}

// fsync(2) of the file or directory at `path`.
static int sync_path(const char *path, int flags) {
	int fd = open(path, flags);
	if(fd < 0)
		return -1;
	int result = fsync(fd);
	int error = errno;
	close(fd);
	errno = error;
	return result;
}

// A name with no file, or -1; a name with one, or -1.
static int free_name(struct worker *w) {
	int start = (int)below(w, NAMES);
	for(int i = 0; i < NAMES; i++)
		if(w->names[(start + i) % NAMES] < 0)
			return (start + i) % NAMES;
	return -1;
}

static int used_name(struct worker *w) {
	int start = (int)below(w, NAMES);
	for(int i = 0; i < NAMES; i++)
		if(w->names[(start + i) % NAMES] >= 0)
			return (start + i) % NAMES;
	return -1;
}

// A new file for `name`, in a random directory.
static int new_file(struct worker *w, int name) {
	for(int i = 0; i < NAMES; i++) {
		struct file *f = &w->files[i];
		if(f->links)
			continue;
		if(!f->data && !(f->data = malloc(MAX_SIZE)))
			return FAIL("malloc(MAX_SIZE)");
		f->size = 0;
		f->links = 1;
		w->names[name] = i;
		w->where[name] = (int)below(w, SUBDIRS);
		return 0;
	}
	errno = 0;
	return FAIL("a free file entry");
}

// Takes `name` off its file, which goes with its last name.
static void drop_name(struct worker *w, int name) {
	w->files[w->names[name]].links--;
	w->names[name] = -1;
}

// Sets the size of `f` to `size`: what it grows by reads as zeros.
static void resize(struct file *f, size_t size) {
	if(size > f->size)
		memset(f->data + f->size, 0, size - f->size);
	f->size = size;
}

// A size for a file written whole: mostly small, some up to MAX_SIZE.
static size_t whole_size(struct worker *w) {
	switch(below(w, 16)) {
	case 0:
		return below(w, MAX_SIZE + 1);
	case 1:
	case 2:
	case 3:
		return below(w, 256 * 1024);
	default:
		return below(w, 16 * 1024);
	}
}

// Writes a name's file whole, making it first if it has none.
static int write_whole(struct worker *w, int name) {
	if(w->names[name] < 0 && new_file(w, name) < 0)
		return -1;
	struct file *f = &w->files[w->names[name]];
	size_t size = whole_size(w);
	fill(f->data, mix(&w->state), 0, size);
	f->size = size;
	if(!w->disk)
		return 0;
	char path[300];
	name_path(w, name, path, sizeof(path));
	int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
	TRY(fd);
	int result = write_all(fd, f->data, size);
	close(fd);
	return result < 0 ? FAIL("writing a file whole") : 0;
}

// Writes part of a name's file, perhaps past its end, leaving a hole.
static int write_part(struct worker *w, int name) {
	struct file *f = &w->files[w->names[name]];
	size_t offset = below(w, min_size(f->size + 65536, MAX_SIZE - 1) + 1);
	size_t len = 1 + below(w, min_size(128 * 1024, MAX_SIZE - offset));
	resize(f, f->size > offset ? f->size : offset);
	fill(f->data, mix(&w->state), offset, len);
	if(offset + len > f->size)
		f->size = offset + len;
	if(!w->disk)
		return 0;
	char path[300];
	name_path(w, name, path, sizeof(path));
	int fd = open(path, O_WRONLY);
	TRY(fd);
	int result = write_at(fd, f->data + offset, len, (off_t)offset);
	close(fd);
	return result < 0 ? FAIL("writing part of a file") : 0;
}

// Appends to a name's file.
static int append(struct worker *w, int name) {
	struct file *f = &w->files[w->names[name]];
	if(f->size == MAX_SIZE)
		return 0;
	size_t len = 1 + below(w, min_size(64 * 1024, MAX_SIZE - f->size));
	fill(f->data, mix(&w->state), f->size, len);
	f->size += len;
	if(!w->disk)
		return 0;
	char path[300];
	name_path(w, name, path, sizeof(path));
	int fd = open(path, O_WRONLY | O_APPEND);
	TRY(fd);
	int result = write_all(fd, f->data + f->size - len, len);
	close(fd);
	return result < 0 ? FAIL("appending to a file") : 0;
}

// Cuts or grows a name's file.
static int cut(struct worker *w, int name) {
	struct file *f = &w->files[w->names[name]];
	size_t size = below(w, min_size(f->size * 2 + 65536, MAX_SIZE) + 1);
	resize(f, size);
	if(!w->disk)
		return 0;
	char path[300];
	name_path(w, name, path, sizeof(path));
	TRY(truncate(path, (off_t)size));
	return 0;
}

// Saves a name's file anew as editors do: written to a temporary file in
// its directory, synced, and renamed over it.
static int save(struct worker *w, int name) {
	struct file *f = &w->files[w->names[name]];
	if(f->links > 1)
		return 0; // The rename would part it from its other names.
	size_t size = whole_size(w);
	fill(f->data, mix(&w->state) | 1, 0, size);
	f->size = size;
	if(!w->disk)
		return 0;
	char path[300], temp[300];
	name_path(w, name, path, sizeof(path));
	snprintf(temp, sizeof(temp), "%s/%s/save.tmp", w->dir, SUBDIR[w->where[name]]);
	int fd = open(temp, O_WRONLY | O_CREAT | O_EXCL, 0644);
	TRY(fd);
	int result = write_all(fd, f->data, size);
	if(result == 0)
		result = fsync(fd);
	close(fd);
	if(result < 0)
		return FAIL("writing and syncing a temporary file");
	TRY(rename(temp, path));
	w->synced++;
	return 0;
}

// Renames a name's file to another name, over its file if it has one
// (where that is), or into a random directory.
static int rename_name(struct worker *w, int from) {
	int to = (int)below(w, NAMES);
	// Renaming a file over another name of its own does nothing.
	if(to == from || w->names[to] == w->names[from])
		return 0;
	char path[300], new_path[300];
	name_path(w, from, path, sizeof(path));
	if(w->names[to] >= 0)
		drop_name(w, to);
	else
		w->where[to] = (int)below(w, SUBDIRS);
	w->names[to] = w->names[from];
	w->names[from] = -1;
	if(!w->disk)
		return 0;
	name_path(w, to, new_path, sizeof(new_path));
	TRY(rename(path, new_path));
	return 0;
}

// Removes a name.
static int remove_name(struct worker *w, int name) {
	char path[300];
	name_path(w, name, path, sizeof(path));
	drop_name(w, name);
	if(w->disk)
		TRY(unlink(path));
	return 0;
}

// Gives a name's file another name, in a random directory.
static int link_name(struct worker *w, int name) {
	int to = free_name(w);
	if(to < 0)
		return 0;
	w->names[to] = w->names[name];
	w->where[to] = (int)below(w, SUBDIRS);
	w->files[w->names[name]].links++;
	if(!w->disk)
		return 0;
	char path[300], new_path[300];
	name_path(w, name, path, sizeof(path));
	name_path(w, to, new_path, sizeof(new_path));
	TRY(link(path, new_path));
	return 0;
}

// Points a symbolic link somewhere new: made beside it and renamed over
// it, or over another of them.
static int relink(struct worker *w) {
	int link = (int)below(w, LINKS);
	char *target = w->links[link];
	size_t len = 1 + below(w, TARGET_MAX);
	uint64_t key = mix(&w->state);
	for(size_t i = 0; i < len; i++) {
		uint64_t at = key + i;
		target[i] = "abcdefghijklmnopqrstuvwxyz0123456789/._-"[mix(&at) % 40];
	}
	target[len] = 0;
	char path[300], new_path[300];
	link_path(w, link, ".new", new_path, sizeof(new_path));
	link_path(w, link, "", path, sizeof(path));
	if(w->disk) {
		TRY(symlink(target, new_path));
		TRY(rename(new_path, path));
	}
	// And now and then one renamed over another.
	if(below(w, 4))
		return 0;
	int over = (int)below(w, LINKS);
	if(over == link)
		return 0;
	memcpy(w->links[over], target, len + 1);
	target[0] = 0;
	if(!w->disk)
		return 0;
	link_path(w, over, "", new_path, sizeof(new_path));
	TRY(rename(path, new_path));
	return 0;
}

// Makes a directory of small files, lists it, and removes it all.
static int scratch(struct worker *w) {
	int count = 1 + (int)below(w, 8);
	if(!w->disk)
		return 0;
	char dir[300], path[320];
	snprintf(dir, sizeof(dir), "%s/scratch", w->dir);
	TRY(mkdir(dir, 0755));
	for(int i = 0; i < count; i++) {
		snprintf(path, sizeof(path), "%s/s%d", dir, i);
		int fd = open(path, O_WRONLY | O_CREAT | O_EXCL, 0644);
		TRY(fd);
		int result = write_all(fd, (const unsigned char *)path, strlen(path));
		close(fd);
		TRY(result);
	}
	DIR *d = opendir(dir);
	if(!d)
		return FAIL("opendir(scratch)");
	int seen = 0;
	for(struct dirent *entry; (entry = readdir(d));)
		seen += entry->d_name[0] != '.';
	closedir(d);
	if(seen != count) {
		errno = 0;
		return FAIL("the scratch directory lists its files");
	}
	for(int i = 0; i < count; i++) {
		snprintf(path, sizeof(path), "%s/s%d", dir, i);
		TRY(unlink(path));
	}
	TRY(rmdir(dir));
	return 0;
}

// fsync(2) of a name's file, or of one of the directories.
static int sync_one(struct worker *w, int name) {
	int subdir = (int)below(w, SUBDIRS);
	if(!w->disk)
		return 0;
	char path[300];
	if(name >= 0) {
		name_path(w, name, path, sizeof(path));
		TRY(sync_path(path, O_RDWR));
	} else {
		snprintf(path, sizeof(path), "%s/%s", w->dir, SUBDIR[subdir]);
		TRY(sync_path(path, O_RDONLY | O_DIRECTORY));
	}
	w->synced++;
	return 0;
}

// Checks that a name's file holds what it should.
static int read_back(struct worker *w, int name) {
	struct file *f = &w->files[w->names[name]];
	char path[300];
	name_path(w, name, path, sizeof(path));
	struct stat st;
	TRY(lstat(path, &st));
	if(!S_ISREG(st.st_mode) || (size_t)st.st_size != f->size || (int)st.st_nlink != f->links) {
		printf("%s: FAILED: %s is %#o, %lld bytes, %d links; not a file of %zu bytes, %d links\n",
				check_name, path, (unsigned)st.st_mode, (long long)st.st_size,
				(int)st.st_nlink, f->size, f->links);
		check_failures++;
		return -1;
	}
	int fd = open(path, O_RDONLY);
	TRY(fd);
	size_t at = 0;
	while(at < f->size) {
		ssize_t n = pread(fd, w->buffer + at, f->size - at, (off_t)at);
		if(n <= 0)
			break;
		at += (size_t)n;
	}
	close(fd);
	if(at != f->size || memcmp(w->buffer, f->data, f->size)) {
		size_t first = 0;
		while(first < at && w->buffer[first] == f->data[first])
			first++;
		printf("%s: FAILED: %s reads %zu of %zu bytes, the first wrong at %zu\n", check_name,
				path, at, f->size, first);
		check_failures++;
		return -1;
	}
	w->read_back++;
	return 0;
}

// One random change, or a file read back, or a sync.
static int change(struct worker *w) {
	unsigned what = (unsigned)below(w, 100);
	int name = used_name(w);
	if(name < 0 || what < 20)
		return write_whole(w, (int)below(w, NAMES));
	if(what < 34)
		return write_part(w, name);
	if(what < 40)
		return append(w, name);
	if(what < 47)
		return cut(w, name);
	if(what < 52)
		return save(w, name);
	if(what < 61)
		return rename_name(w, name);
	if(what < 69)
		return remove_name(w, name);
	if(what < 74)
		return link_name(w, name);
	if(what < 79)
		return relink(w);
	if(what < 82)
		return scratch(w);
	if(what < 87)
		return sync_one(w, name);
	if(what < 90)
		return sync_one(w, -1);
	return w->disk ? read_back(w, name) : 0;
}

// Checks that the directory `dir` holds the names `expected` gives and no
// others, `count` of them.
static int check_listing(struct worker *w, const char *dir, int (*expected)(struct worker *,
		const char *), int count) {
	DIR *d = opendir(dir);
	if(!d)
		return FAIL("opendir(dir)");
	int seen = 0, wrong = 0;
	for(struct dirent *entry; (entry = readdir(d));) {
		if(!strcmp(entry->d_name, ".") || !strcmp(entry->d_name, ".."))
			continue;
		seen++;
		if(!expected(w, entry->d_name)) {
			printf("%s: FAILED: %s holds %s, which it should not\n", check_name, dir,
					entry->d_name);
			wrong++;
		}
	}
	closedir(d);
	if(seen != count && !wrong)
		printf("%s: FAILED: %s holds %d entries, not %d\n", check_name, dir, seen, count);
	if(wrong || seen != count) {
		check_failures++;
		return -1;
	}
	return 0;
}

// The subdirectory check_listing is listing, for in_subdir.
static int listing;

static int in_subdir(struct worker *w, const char *entry) {
	char *end;
	if(entry[0] != 'n' || !entry[1])
		return 0;
	long name = strtol(entry + 1, &end, 10);
	return !*end && name >= 0 && name < NAMES && w->names[name] >= 0 &&
			w->where[name] == listing;
}

static int in_top(struct worker *w, const char *entry) {
	for(int i = 0; i < SUBDIRS; i++)
		if(!strcmp(entry, SUBDIR[i]))
			return 1;
	char *end;
	if(entry[0] != 'l' || !entry[1])
		return 0;
	long link = strtol(entry + 1, &end, 10);
	return !*end && link >= 0 && link < LINKS && w->links[link][0];
}

// Checks everything in the worker's directory: each name's file, the
// files that share names, each symbolic link, and nothing more.
static int check_all(struct worker *w) {
	int result = 0;
	ino_t numbers[NAMES] = {0};
	for(int name = 0; name < NAMES; name++) {
		if(w->names[name] < 0)
			continue;
		if(read_back(w, name) < 0) {
			result = -1;
			continue;
		}
		char path[300];
		name_path(w, name, path, sizeof(path));
		struct stat st;
		if(lstat(path, &st) < 0)
			return FAIL("lstat(path)");
		ino_t *number = &numbers[w->names[name]];
		if(*number && *number != st.st_ino) {
			printf("%s: FAILED: %s is not the file its other names are\n", check_name, path);
			check_failures++;
			result = -1;
		}
		*number = st.st_ino;
	}
	char path[300];
	int top = SUBDIRS;
	for(int link = 0; link < LINKS; link++) {
		if(!w->links[link][0])
			continue;
		top++;
		char target[TARGET_MAX + 2];
		link_path(w, link, "", path, sizeof(path));
		ssize_t len = readlink(path, target, sizeof(target));
		if(len < 0 || (size_t)len != strlen(w->links[link]) ||
				memcmp(target, w->links[link], (size_t)len)) {
			printf("%s: FAILED: the symbolic link %s does not point to %s\n", check_name, path,
					w->links[link]);
			check_failures++;
			result = -1;
		}
	}
	if(check_listing(w, w->dir, in_top, top) < 0)
		result = -1;
	for(listing = 0; listing < SUBDIRS; listing++) {
		int count = 0;
		for(int name = 0; name < NAMES; name++)
			count += w->names[name] >= 0 && w->where[name] == listing;
		snprintf(path, sizeof(path), "%s/%s", w->dir, SUBDIR[listing]);
		if(check_listing(w, path, in_subdir, count) < 0)
			result = -1;
	}
	return result;
}

// Syncs every file and directory of the worker's.
static int sync_all(struct worker *w) {
	char path[300];
	for(int name = 0; name < NAMES; name++) {
		if(w->names[name] < 0)
			continue;
		name_path(w, name, path, sizeof(path));
		TRY(sync_path(path, O_RDWR));
	}
	for(int i = 0; i < SUBDIRS; i++) {
		snprintf(path, sizeof(path), "%s/%s", w->dir, SUBDIR[i]);
		TRY(sync_path(path, O_RDONLY | O_DIRECTORY));
	}
	TRY(sync_path(w->dir, O_RDONLY | O_DIRECTORY));
	return 0;
}

// Removes everything of the worker's, and its directory.
static int remove_all(struct worker *w) {
	char path[300];
	for(int name = 0; name < NAMES; name++) {
		if(w->names[name] < 0)
			continue;
		name_path(w, name, path, sizeof(path));
		TRY(unlink(path));
	}
	for(int link = 0; link < LINKS; link++) {
		if(!w->links[link][0])
			continue;
		link_path(w, link, "", path, sizeof(path));
		TRY(unlink(path));
	}
	for(int i = 0; i < SUBDIRS; i++) {
		snprintf(path, sizeof(path), "%s/%s", w->dir, SUBDIR[i]);
		TRY(rmdir(path));
	}
	TRY(rmdir(w->dir));
	return 0;
}

// Sets up worker `number` in `dir`, from `seed`.
static int start(struct worker *w, const char *dir, int number, uint64_t seed, int disk) {
	memset(w, 0, sizeof(*w));
	snprintf(w->dir, sizeof(w->dir), "%s/torture/w%d", dir, number % WORKERS);
	w->number = number;
	w->disk = disk;
	w->state = seed ^ (uint64_t)(number + 1) * 0xd1342543de82ef95ull;
	for(int name = 0; name < NAMES; name++)
		w->names[name] = -1;
	if(!(w->buffer = malloc(MAX_SIZE)))
		return FAIL("malloc(MAX_SIZE)");
	return 0;
}

static void finish(struct worker *w) {
	for(int i = 0; i < NAMES; i++)
		free(w->files[i].data);
	free(w->buffer);
}

// The file beside a worker's directory that says how many changes it
// made.
static void count_path(const struct worker *w, char *path, size_t size) {
	snprintf(path, size, "%s.changes", w->dir);
}

// A worker's run: its changes for `seconds`, then everything checked and
// synced, and how many changes it made.
static void run(const char *dir, int number, uint64_t seed, long seconds) {
	static struct worker w;
	if(start(&w, dir, number, seed, 1) < 0)
		return;
	char path[300];
	CHECK(mkdir(w.dir, 0755) == 0);
	for(int i = 0; i < SUBDIRS; i++) {
		snprintf(path, sizeof(path), "%s/%s", w.dir, SUBDIR[i]);
		CHECK(mkdir(path, 0755) == 0);
	}
	if(check_failures)
		return;
	long long began = check_now_ns(), second = 1000000000LL;
	long long end = began + seconds * second, quarter = began + seconds * second / 4;
	for(long long now = began; now < end; now = check_now_ns()) {
		if(change(&w) < 0) {
			printf("%s: %s: change %ld failed\n", check_name, w.dir, w.changes + 1);
			return;
		}
		w.changes++;
		if(now >= quarter) {
			printf("%s: %s: %ld changes in %lld s\n", check_name, w.dir, w.changes,
					(now - began) / second);
			quarter += seconds * second / 4;
		}
	}
	if(check_all(&w) < 0 || sync_all(&w) < 0)
		return;
	count_path(&w, path, sizeof(path));
	char text[32];
	int len = snprintf(text, sizeof(text), "%ld\n", w.changes);
	int fd = open(path, O_WRONLY | O_CREAT | O_EXCL, 0644);
	CHECK(fd >= 0 && write_all(fd, (const unsigned char *)text, (size_t)len) == 0 &&
			fsync(fd) == 0);
	close(fd);
	if(!check_failures)
		printf("%s: %s: %ld changes, %ld syncs, %ld files read back; "
				"everything checked and synced\n",
				check_name, w.dir, w.changes, w.synced, w.read_back);
	finish(&w);
}

// A worker's check after the reboot: as many changes as it made worked
// out again, the disk checked against them, and everything removed.
static void recheck(const char *dir, int number, uint64_t seed) {
	static struct worker w;
	if(start(&w, dir, number, seed, 0) < 0)
		return;
	char path[300], text[32] = "";
	count_path(&w, path, sizeof(path));
	int fd = open(path, O_RDONLY);
	CHECK(fd >= 0 && read(fd, text, sizeof(text) - 1) > 0);
	close(fd);
	long changes = strtol(text, NULL, 10);
	CHECK(changes > 0);
	for(w.changes = 0; w.changes < changes; w.changes++)
		if(change(&w) < 0)
			return;
	int files = 0;
	for(int name = 0; name < NAMES; name++)
		files += w.names[name] >= 0;
	if(check_all(&w) == 0) {
		printf("%s: %s: %d files read back after the reboot, every byte as written\n",
				check_name, w.dir, files);
		if(remove_all(&w) == 0 && unlink(path) == 0)
			printf("%s: %s: removed\n", check_name, w.dir);
		else
			check_failed(__FILE__, __LINE__, "removing it all", errno);
	}
	finish(&w);
}

int main(int argc, char **argv) {
	check_start("torture");
	int checking = argc > 1 && !strcmp(argv[1], "--check");
	if(argc < 4) {
		printf("torture: FAILED: say `torture <seed> <seconds> <dir>...` or "
				"`torture --check <seed> <dir>...`\n");
		return 1;
	}
	uint64_t seed = strtoull(argv[2 - !checking], NULL, 0);
	char **dirs = argv + 3;
	int count = argc - 3;
	char path[300];
	if(!checking) {
		long seconds = strtol(argv[2], NULL, 0);
		for(int i = 0; i < count; i++) {
			snprintf(path, sizeof(path), "%s/torture", dirs[i]);
			CHECK(mkdir(path, 0755) == 0);
		}
		if(check_failures)
			return check_end();
		printf("torture: %d workers making changes for %ld s, seed %#llx\n", count * WORKERS,
				seconds, (unsigned long long)seed);
		long long began = check_now_ns();
		pid_t children[count * WORKERS];
		for(int number = 0; number < count * WORKERS; number++) {
			children[number] = check_fork();
			if(children[number] == 0) {
				run(dirs[number / WORKERS], number, seed, seconds);
				check_exit_child();
			}
		}
		for(int number = 0; number < count * WORKERS; number++)
			if(children[number] > 0)
				CHECK_CHILD(children[number]);
		for(int i = 0; i < count; i++) {
			snprintf(path, sizeof(path), "%s/torture", dirs[i]);
			CHECK(sync_path(path, O_RDONLY | O_DIRECTORY) == 0);
			CHECK(sync_path(dirs[i], O_RDONLY | O_DIRECTORY) == 0);
		}
		printf("torture: done in %lld s\n", (check_now_ns() - began) / 1000000000LL);
		return check_end();
	}
	for(int number = 0; number < count * WORKERS; number++)
		recheck(dirs[number / WORKERS], number, seed);
	for(int i = 0; i < count; i++) {
		snprintf(path, sizeof(path), "%s/torture", dirs[i]);
		CHECK(rmdir(path) == 0);
		CHECK(sync_path(dirs[i], O_RDONLY | O_DIRECTORY) == 0);
	}
	return check_end();
}
