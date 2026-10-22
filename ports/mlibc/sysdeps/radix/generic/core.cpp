// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// The Radix system layer's core: logging, memory, threads, futexes,
// clocks and what the system is (ADR 0068).

#include <abi-bits/vm-flags.h>
#include <bits/ensure.h>
#include <errno.h>
#include <limits.h>
#include <mlibc/all-sysdeps.hpp>
#include <mlibc/debug.hpp>
#include <mlibc/tcb.hpp>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/utsname.h>
#include <time.h>
#include <unistd.h>

#include "radix.hpp"

// Where new threads start (x86_64/thread-entry.S).
#ifndef MLIBC_BUILDING_RTLD
extern "C" void __mlibc_thread_entry();
#endif

// frigg's assertions call frg_panic (options/internal/generic/frigg.cpp)
// through a weak reference, which does not take it out of a static
// libc.a, so a failed one would trap without a word. This reference does.
extern "C" void frg_panic(const char *message);
[[gnu::used]] static void (*const keep_frg_panic)(const char *) = frg_panic;

namespace mlibc {

void Sysdeps<LibcLog>::operator()(const char *message) {
	// To standard error, where a program's user sees it, and to the kernel
	// log if that fails (before standard error is open, say).
	size_t len = strlen(message);
	ssize_t written;
	char newline = '\n';
	if(sysdep<Write>(2, message, len, &written) || sysdep<Write>(2, &newline, 1, &written))
		radix::log(message, len);
}

[[noreturn]] void Sysdeps<LibcPanic>::operator()() {
	sysdep<LibcLog>("mlibc: panic; ending the process with SIGABRT");
	radix_process_kill(radix::SLOT_PROCESS, static_cast<int64_t>(radix::pid()), SIGABRT);
	radix_process_exit(radix::SLOT_PROCESS, 128 + SIGABRT);
	__builtin_trap();
}

[[noreturn]] void Sysdeps<Exit>::operator()(int status) {
	radix_process_exit(radix::SLOT_PROCESS, static_cast<uint64_t>(status) & 0xff);
	__builtin_trap();
}

[[noreturn]] void Sysdeps<ThreadExit>::operator()() {
	radix_process_exit_thread(radix::SLOT_PROCESS, 0);
	__builtin_trap();
}

int Sysdeps<TcbSet>::operator()(void *pointer) {
	// x86-64's thread pointer points at the TCB itself (its first word
	// points to itself).
	return radix::to_errno(radix_process_set_thread_pointer(radix::SLOT_PROCESS,
			reinterpret_cast<uint64_t>(pointer)));
}

pid_t Sysdeps<FutexTid>::operator()() {
	struct radix_process_thread_id_results r;
	if(radix_process_thread_id(radix::SLOT_PROCESS, &r))
		return 0;
	return static_cast<pid_t>(r.thread);
}

pid_t Sysdeps<GetTid>::operator()() {
	return sysdep<FutexTid>();
}

// The dynamic linker is built from this file too, but without the rest of
// the C library: it starts no threads, and has none of the string
// functions what follows uses.
#ifndef MLIBC_BUILDING_RTLD
int Sysdeps<Clone>::operator()(void *tcb, pid_t *pid_out, void *stack) {
	(void)tcb;
	struct radix_process_start_results r;
	uint64_t status = radix_process_start(radix::SLOT_PROCESS,
			reinterpret_cast<uint64_t>(&__mlibc_thread_entry),
			reinterpret_cast<uint64_t>(stack), 0, radix::NO_SLOT, &r);
	if(status)
		return radix::to_errno(status);
	*pid_out = static_cast<pid_t>(r.thread);
	return 0;
}
#endif

void Sysdeps<Yield>::operator()() {
	radix_yield();
}

namespace {

// The rights of memory with `prot`'s access.
uint64_t rights_of(int prot) {
	uint64_t rights = 0;
	if(prot & PROT_READ)
		rights |= RADIX_RIGHTS_READ;
	if(prot & PROT_WRITE)
		rights |= RADIX_RIGHTS_READ | RADIX_RIGHTS_WRITE;
	if(prot & PROT_EXEC)
		rights |= RADIX_RIGHTS_READ | RADIX_RIGHTS_EXECUTE;
	return rights;
}

// Nanoseconds in `ts`, at most UINT64_MAX.
uint64_t ns_of(const struct timespec *ts) {
	if(ts->tv_sec < 0)
		return 0;
	uint64_t secs = static_cast<uint64_t>(ts->tv_sec);
	if(secs >= UINT64_MAX / 1000000000 - 1)
		return UINT64_MAX;
	return secs * 1000000000 + static_cast<uint64_t>(ts->tv_nsec);
}

} // namespace

int Sysdeps<AnonAllocate>::operator()(size_t size, void **pointer) {
	return sysdep<VmMap>(nullptr, size, PROT_READ | PROT_WRITE,
			MAP_PRIVATE | MAP_ANONYMOUS, -1, 0, pointer);
}

int Sysdeps<AnonFree>::operator()(void *pointer, size_t size) {
	return sysdep<VmUnmap>(pointer, size);
}

int Sysdeps<VmMap>::operator()(void *hint, size_t size, int prot, int flags, int fd, off_t offset,
		void **window) {
	if(!size)
		return EINVAL;
	uint32_t map_flags = 0;
	if(flags & MAP_FIXED)
		map_flags |= RADIX_MAP_FLAGS_FIXED;
	// Linux's MAP_FIXED_NOREPLACE, which <sys/mman.h> has only with
	// mlibc's Linux interfaces.
	if(flags & 0x100000)
		map_flags |= RADIX_MAP_FLAGS_FIXED_NO_REPLACE;
	uint64_t rights = rights_of(prot);
	uint64_t address = reinterpret_cast<uint64_t>(hint);
	if(flags & MAP_ANONYMOUS) {
		if(flags & MAP_SHARED)
			map_flags |= RADIX_MAP_FLAGS_SHARED;
		struct radix_address_space_map_anonymous_results r;
		uint64_t status = radix_address_space_map_anonymous(radix::SLOT_SPACE, address, size,
				rights, map_flags, &r);
		if(status)
			return radix::to_errno(status);
		*window = reinterpret_cast<void *>(r.address);
		return 0;
	}
	// A file's pages are the process's own copy: a shared mapping can
	// only be one that nobody writes through (docs/design/posix.md).
	if((flags & MAP_SHARED) && (prot & PROT_WRITE))
		return ENODEV;
	uint32_t slot = radix::slot_of(fd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	if(offset < 0 || (offset & 4095))
		return EINVAL;
	struct radix_address_space_map_file_results r;
	uint64_t status = radix_address_space_map_file(radix::SLOT_SPACE, address, size, rights,
			map_flags, slot, static_cast<uint64_t>(offset), &r);
	if(status)
		return radix::to_errno(status);
	*window = reinterpret_cast<void *>(r.address);
	return 0;
}

int Sysdeps<VmUnmap>::operator()(void *pointer, size_t size) {
	return radix::to_errno(radix_address_space_discard(radix::SLOT_SPACE,
			reinterpret_cast<uint64_t>(pointer), size));
}

int Sysdeps<VmProtect>::operator()(void *pointer, size_t size, int prot) {
	return radix::to_errno(radix_address_space_protect(radix::SLOT_SPACE,
			reinterpret_cast<uint64_t>(pointer), size, rights_of(prot)));
}

int Sysdeps<Madvise>::operator()(void *addr, size_t length, int advice) {
	(void)addr;
	(void)length;
	(void)advice;
	// Advice only.
	return 0;
}

int Sysdeps<PosixMadvise>::operator()(void *addr, size_t length, int advice) {
	return sysdep<Madvise>(addr, length, advice);
}

int Sysdeps<Msync>::operator()(void *addr, size_t length, int flags) {
	(void)addr;
	(void)length;
	(void)flags;
	// Mappings of files are private copies: nothing to write back.
	return 0;
}

int Sysdeps<FutexWait>::operator()(int *pointer, int expected, const struct timespec *time) {
	uint64_t timeout = 0;
	if(time) {
		timeout = ns_of(time);
		// 0 means no limit to the kernel: the shortest wait instead.
		if(!timeout)
			timeout = 1;
	}
	uint64_t status = radix_address_space_futex_wait(radix::SLOT_SPACE,
			reinterpret_cast<uint64_t>(pointer), static_cast<uint32_t>(expected), timeout);
	return radix::to_errno(status);
}

int Sysdeps<FutexWake>::operator()(int *pointer, bool all) {
	struct radix_address_space_futex_wake_results r;
	return radix::to_errno(radix_address_space_futex_wake(radix::SLOT_SPACE,
			reinterpret_cast<uint64_t>(pointer), all ? INT32_MAX : 1, &r));
}

int Sysdeps<ClockGet>::operator()(int clock, time_t *secs, long *nanos) {
	// Linux's clock numbers are the kernel's ClockKind.
	if(clock < 0 || clock > RADIX_CLOCK_KIND_BOOTTIME)
		return EINVAL;
	struct radix_process_clock_results r;
	uint64_t status = radix_process_clock(radix::SLOT_PROCESS,
			static_cast<enum radix_clock_kind>(clock), &r);
	if(status)
		return radix::to_errno(status);
	*secs = static_cast<time_t>(r.ns / 1000000000);
	*nanos = static_cast<long>(r.ns % 1000000000);
	return 0;
}

int Sysdeps<ClockGetres>::operator()(int clock, time_t *secs, long *nanos) {
	if(clock < 0 || clock > RADIX_CLOCK_KIND_BOOTTIME)
		return EINVAL;
	*secs = 0;
	*nanos = 1;
	return 0;
}

int Sysdeps<ClockSet>::operator()(int clock, time_t secs, long nanos) {
	(void)clock;
	(void)secs;
	(void)nanos;
	return EPERM;
}

int Sysdeps<Sleep>::operator()(time_t *secs, long *nanos) {
	struct timespec want = {*secs, *nanos};
	uint64_t ns = ns_of(&want);
	if(!ns)
		return 0;
	time_t start_secs;
	long start_nanos;
	sysdep<ClockGet>(CLOCK_MONOTONIC, &start_secs, &start_nanos);
	// A futex nobody wakes: the wait ends when the time has passed, or
	// when a signal comes.
	int word = 0;
	uint64_t status = radix_address_space_futex_wait(radix::SLOT_SPACE,
			reinterpret_cast<uint64_t>(&word), 0, ns);
	int e = radix::to_errno(status);
	if(e == ETIMEDOUT || !e) {
		*secs = 0;
		*nanos = 0;
		return 0;
	}
	if(e != EINTR)
		return e;
	time_t now_secs;
	long now_nanos;
	sysdep<ClockGet>(CLOCK_MONOTONIC, &now_secs, &now_nanos);
	uint64_t passed = static_cast<uint64_t>(now_secs - start_secs) * 1000000000
			+ static_cast<uint64_t>(now_nanos) - static_cast<uint64_t>(start_nanos);
	uint64_t left = passed < ns ? ns - passed : 0;
	*secs = static_cast<time_t>(left / 1000000000);
	*nanos = static_cast<long>(left % 1000000000);
	return EINTR;
}

int Sysdeps<GetEntropy>::operator()(void *buffer, size_t length) {
	auto bytes = static_cast<char *>(buffer);
	while(length) {
		size_t n = length < 256 ? length : 256;
		if(uint64_t status = radix_random_read(radix::SLOT_RANDOM, bytes, n); status)
			return radix::to_errno(status);
		bytes += n;
		length -= n;
	}
	return 0;
}

#ifndef MLIBC_BUILDING_RTLD
namespace {

// Reads the first line of a file of /sys into `out` (`size` bytes, with
// its NUL), without the newline. Returns whether it could.
bool read_sys(const char *path, char *out, size_t size) {
	int fd;
	if(sysdep<Open>(path, O_RDONLY | O_CLOEXEC, 0, &fd))
		return false;
	ssize_t got = 0;
	int e = sysdep<Read>(fd, out, size - 1, &got);
	sysdep<Close>(fd);
	if(e || got <= 0)
		return false;
	out[got] = 0;
	if(char *newline = strchr(out, '\n'); newline)
		*newline = 0;
	return true;
}

} // namespace

int Sysdeps<Uname>::operator()(struct utsname *buf) {
	memset(buf, 0, sizeof(*buf));
	strncpy(buf->sysname, "Dukovo", sizeof(buf->sysname) - 1);
	if(sysdep<GetHostname>(buf->nodename, sizeof(buf->nodename)))
		strncpy(buf->nodename, "dukovo", sizeof(buf->nodename) - 1);
	char version[64];
	if(!read_sys("/sys/kernel/version", version, sizeof(version)))
		strcpy(version, "unknown");
	strncpy(buf->release, version, sizeof(buf->release) - 1);
	strncpy(buf->version, "Radix", sizeof(buf->version) - 1);
	strncpy(buf->machine, "x86_64", sizeof(buf->machine) - 1);
	return 0;
}

int Sysdeps<GetHostname>::operator()(char *buffer, size_t bufsize) {
	// Naming the machine comes with networking (Phase 12).
	const char name[] = "dukovo";
	if(bufsize < sizeof(name))
		return ENAMETOOLONG;
	memcpy(buffer, name, sizeof(name));
	return 0;
}

int Sysdeps<SetHostname>::operator()(const char *buffer, size_t bufsize) {
	(void)buffer;
	(void)bufsize;
	return EPERM;
}

int Sysdeps<Sysconf>::operator()(int num, long *ret) {
	switch(num) {
	case _SC_NPROCESSORS_CONF:
	case _SC_NPROCESSORS_ONLN: {
		char cpus[32];
		*ret = 1;
		if(read_sys("/sys/kernel/cpus", cpus, sizeof(cpus))) {
			long n = strtol(cpus, nullptr, 10);
			if(n > 0)
				*ret = n;
		}
		return 0;
	}
	case _SC_OPEN_MAX:
		*ret = radix::files_limit();
		return 0;
	case _SC_PAGESIZE:
		*ret = 4096;
		return 0;
	case _SC_CLK_TCK:
		*ret = 100;
		return 0;
	default:
		// mlibc's own answer.
		return EINVAL;
	}
}
#endif

int Sysdeps<GetRlimit>::operator()(int resource, struct rlimit *limit) {
	// The RLIMIT_ numbers are the kernel's LimitKind, and RLIM_INFINITY
	// its UNLIMITED.
	if(resource < 0)
		return EINVAL;
	struct radix_process_limits_results r;
	if(uint64_t status = radix_process_limits(radix::SLOT_PROCESS,
			static_cast<enum radix_limit_kind>(resource), false, 0, 0, &r); status)
		return radix::to_errno(status);
	limit->rlim_cur = r.current;
	limit->rlim_max = r.max;
	return 0;
}

int Sysdeps<SetRlimit>::operator()(int resource, const struct rlimit *limit) {
	if(resource < 0)
		return EINVAL;
	struct radix_process_limits_results r;
	if(uint64_t status = radix_process_limits(radix::SLOT_PROCESS,
			static_cast<enum radix_limit_kind>(resource), true, limit->rlim_cur,
			limit->rlim_max, &r); status)
		return radix::to_errno(status);
	if(resource == RLIMIT_NOFILE)
		radix::set_files_limit(limit->rlim_cur);
	return 0;
}

int Sysdeps<GetCurrentStackInfo>::operator()(void **stack_base, size_t *stack_size) {
	// Only the first thread asks: pthread_getattr_np knows its other
	// threads' stacks.
	return radix::first_stack(stack_base, stack_size);
}

} // namespace mlibc
