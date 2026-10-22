// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Processes and signals on Radix (ADR 0061 to ADR 0063, ADR 0068): fork,
// exec and wait, process groups and sessions, signal actions, masks and
// stacks, timers and CPU times, all methods of the process's own
// capability (radix-process(2)).

#include <bits/ensure.h>
#include <errno.h>
#include <fcntl.h>
#include <mlibc/all-sysdeps.hpp>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/time.h>
#include <sys/times.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include "radix.hpp"

// Where a signal handler returns to (x86_64/signal.S).
extern "C" void __radix_signal_restorer();

namespace {

constexpr uint64_t NS_PER_SEC = 1000000000;

// Nanoseconds in `tv`.
uint64_t ns_of(const struct timeval &tv) {
	if(tv.tv_sec < 0)
		return 0;
	return static_cast<uint64_t>(tv.tv_sec) * NS_PER_SEC + static_cast<uint64_t>(tv.tv_usec) * 1000;
}

struct timeval timeval_of(uint64_t ns) {
	struct timeval tv;
	tv.tv_sec = static_cast<time_t>(ns / NS_PER_SEC);
	tv.tv_usec = static_cast<suseconds_t>(ns % NS_PER_SEC / 1000);
	return tv;
}

// A set of signals as the kernel takes one: bit n - 1 for signal n.
uint64_t mask_of(const sigset_t *set) {
	uint64_t mask;
	memcpy(&mask, set, sizeof(mask));
	return mask;
}

void set_of(uint64_t mask, sigset_t *set) {
	memset(set, 0, sizeof(*set));
	memcpy(set, &mask, sizeof(mask));
}

// Appends `text` and its NUL to `block` at `*at`, if it fits in `size`.
bool put(char *block, size_t size, size_t *at, const char *text) {
	size_t len = strlen(text) + 1;
	if(size - *at < len)
		return false;
	memcpy(block + *at, text, len);
	*at += len;
	return true;
}

// Runs the program in descriptor `fd` in this process, as execve does.
int exec_fd(int fd, const char *execfn, char *const argv[], char *const envp[]) {
	uint32_t slot = radix::slot_of(fd);
	if(slot == radix::NO_SLOT)
		return EBADF;
	size_t args = 0;
	size_t size = sizeof(uint32_t) * 4;
	for(; argv[args]; args++)
		size += strlen(argv[args]) + 1;
	size_t envs = 0;
	for(; envp[envs]; envs++)
		size += strlen(envp[envs]) + 1;
	const char *cwd = radix::cwd();
	size += strlen(cwd) + 1 + strlen(execfn) + 1;
	if(size > RADIX_PROCESS_EXEC_MAX)
		return E2BIG;
	auto block = static_cast<char *>(malloc(size));
	if(!block)
		return ENOMEM;
	uint32_t header[4] = {RADIX_PROCESS_EXEC_VERSION, static_cast<uint32_t>(args),
			static_cast<uint32_t>(envs), 2};
	memcpy(block, header, sizeof(header));
	size_t at = sizeof(header);
	for(size_t i = 0; i < args; i++)
		put(block, size, &at, argv[i]);
	for(size_t i = 0; i < envs; i++)
		put(block, size, &at, envp[i]);
	// The working directory (AT_RADIX_CWD) and the path it was run as
	// (AT_EXECFN).
	put(block, size, &at, cwd);
	put(block, size, &at, execfn);
	uint64_t status = radix_process_exec(radix::SLOT_PROCESS, slot, block, at);
	free(block);
	// POSIX's errno for a directory or other capability that is not a
	// file to run.
	if(RADIX_STATUS_DOMAIN(status) == RADIX_KERNEL_ERROR_DOMAIN
			&& RADIX_STATUS_CODE(status) == RADIX_KERNEL_WRONG_KIND)
		return EACCES;
	int e = radix::to_errno(status);
	return e == EISDIR ? EACCES : e;
}

} // namespace

namespace mlibc {

int Sysdeps<Fork>::operator()(pid_t *child) {
	struct radix_process_fork_results r;
	uint64_t status = radix_process_fork(radix::SLOT_PROCESS, &r);
	if(status)
		return radix::to_errno(status);
	if(!r.child)
		radix::forget_pid();
	*child = static_cast<pid_t>(r.child);
	return 0;
}

int Sysdeps<Execve>::operator()(const char *path, char *const argv[], char *const envp[]) {
	int fd;
	if(int e = sysdep<Open>(path, O_RDONLY | O_CLOEXEC, 0, &fd); e)
		return e;
	int e = exec_fd(fd, path, argv, envp);
	sysdep<Close>(fd);
	return e;
}

int Sysdeps<Fexecve>::operator()(int fd, char *const argv[], char *const envp[]) {
	char execfn[32] = "/dev/fd/";
	char digits[12];
	int n = 0;
	for(unsigned value = static_cast<unsigned>(fd); n == 0 || value; value /= 10)
		digits[n++] = static_cast<char>('0' + value % 10);
	size_t at = strlen(execfn);
	while(n)
		execfn[at++] = digits[--n];
	execfn[at] = 0;
	return exec_fd(fd, execfn, argv, envp);
}

int Sysdeps<Waitpid>::operator()(pid_t pid, int *status, int flags, struct rusage *ru,
		pid_t *ret_pid) {
	uint32_t wait_flags = 0;
	if(flags & WNOHANG)
		wait_flags |= RADIX_CHILD_WAIT_FLAGS_NO_HANG;
	if(flags & WUNTRACED)
		wait_flags |= RADIX_CHILD_WAIT_FLAGS_STOPPED;
	if(flags & WCONTINUED)
		wait_flags |= RADIX_CHILD_WAIT_FLAGS_CONTINUED;
	if(flags & WNOWAIT)
		wait_flags |= RADIX_CHILD_WAIT_FLAGS_NO_REAP;
	struct radix_process_wait_results r;
	uint64_t result = radix_process_wait(radix::SLOT_PROCESS, pid, wait_flags, &r);
	if(result)
		return radix::to_errno(result);
	if(status)
		*status = static_cast<int>(r.status);
	if(ru) {
		memset(ru, 0, sizeof(*ru));
		ru->ru_utime = timeval_of(r.user_ns);
		ru->ru_stime = timeval_of(r.system_ns);
	}
	*ret_pid = static_cast<pid_t>(r.pid);
	return 0;
}

int Sysdeps<Kill>::operator()(pid_t pid, int sig) {
	if(sig < 0 || sig > 64)
		return EINVAL;
	return radix::to_errno(radix_process_kill(radix::SLOT_PROCESS, pid,
			static_cast<uint32_t>(sig)));
}

int Sysdeps<Tgkill>::operator()(int tgid, int tid, int sig) {
	if(static_cast<uint64_t>(tgid) != radix::pid())
		return ESRCH;
	if(sig < 0 || sig > 64)
		return EINVAL;
	return radix::to_errno(radix_process_signal_thread(radix::SLOT_PROCESS,
			static_cast<uint64_t>(tid), static_cast<uint32_t>(sig)));
}

int Sysdeps<Sigqueue>::operator()(pid_t pid, int sig, const union sigval val) {
	// The value is not carried yet (docs/design/posix.md): the signal is.
	(void)val;
	return sysdep<Kill>(pid, sig);
}

pid_t Sysdeps<GetPid>::operator()() {
	return static_cast<pid_t>(radix::pid());
}

pid_t Sysdeps<GetPpid>::operator()() {
	struct radix_process_ids_results ids;
	if(radix_process_ids(radix::SLOT_PROCESS, 0, &ids))
		return 0;
	return static_cast<pid_t>(ids.parent);
}

int Sysdeps<GetPgid>::operator()(pid_t pid, pid_t *pgid) {
	if(pid < 0)
		return EINVAL;
	struct radix_process_ids_results ids;
	if(uint64_t status = radix_process_ids(radix::SLOT_PROCESS, pid, &ids); status)
		return radix::to_errno(status);
	*pgid = static_cast<pid_t>(ids.group);
	return 0;
}

int Sysdeps<GetSid>::operator()(pid_t pid, pid_t *sid) {
	if(pid < 0)
		return EINVAL;
	struct radix_process_ids_results ids;
	if(uint64_t status = radix_process_ids(radix::SLOT_PROCESS, pid, &ids); status)
		return radix::to_errno(status);
	*sid = static_cast<pid_t>(ids.session);
	return 0;
}

int Sysdeps<SetPgid>::operator()(pid_t pid, pid_t pgid) {
	if(pid < 0 || pgid < 0)
		return EINVAL;
	return radix::to_errno(radix_process_set_group(radix::SLOT_PROCESS, pid, pgid));
}

int Sysdeps<SetSid>::operator()(pid_t *sid) {
	struct radix_process_new_session_results r;
	if(uint64_t status = radix_process_new_session(radix::SLOT_PROCESS, &r); status)
		return radix::to_errno(status);
	*sid = static_cast<pid_t>(r.session);
	return 0;
}

// Users come in Phase 13: until then every program is user 0, group 0,
// and may become only that.

uid_t Sysdeps<GetUid>::operator()() { return 0; }
uid_t Sysdeps<GetEuid>::operator()() { return 0; }
gid_t Sysdeps<GetGid>::operator()() { return 0; }
gid_t Sysdeps<GetEgid>::operator()() { return 0; }

int Sysdeps<SetUid>::operator()(uid_t uid) { return uid ? EPERM : 0; }
int Sysdeps<SetEuid>::operator()(uid_t euid) { return euid ? EPERM : 0; }
int Sysdeps<SetGid>::operator()(gid_t gid) { return gid ? EPERM : 0; }
int Sysdeps<SetEgid>::operator()(gid_t egid) { return egid ? EPERM : 0; }

int Sysdeps<SetResuid>::operator()(uid_t ruid, uid_t euid, uid_t suid) {
	auto ok = [](uid_t id) { return id == 0 || id == static_cast<uid_t>(-1); };
	return ok(ruid) && ok(euid) && ok(suid) ? 0 : EPERM;
}

int Sysdeps<SetResgid>::operator()(gid_t rgid, gid_t egid, gid_t sgid) {
	auto ok = [](gid_t id) { return id == 0 || id == static_cast<gid_t>(-1); };
	return ok(rgid) && ok(egid) && ok(sgid) ? 0 : EPERM;
}

int Sysdeps<GetResuid>::operator()(uid_t *ruid, uid_t *euid, uid_t *suid) {
	*ruid = *euid = *suid = 0;
	return 0;
}

int Sysdeps<GetResgid>::operator()(gid_t *rgid, gid_t *egid, gid_t *sgid) {
	*rgid = *egid = *sgid = 0;
	return 0;
}

int Sysdeps<SetReuid>::operator()(uid_t ruid, uid_t euid) {
	return sysdep<SetResuid>(ruid, euid, static_cast<uid_t>(-1));
}

int Sysdeps<SetRegid>::operator()(gid_t rgid, gid_t egid) {
	return sysdep<SetResgid>(rgid, egid, static_cast<gid_t>(-1));
}

int Sysdeps<GetGroups>::operator()(size_t size, gid_t *list, int *ret) {
	(void)size;
	(void)list;
	*ret = 0;
	return 0;
}

int Sysdeps<SetGroups>::operator()(size_t size, const gid_t *list) {
	(void)list;
	return size ? EPERM : 0;
}

int Sysdeps<GetPriority>::operator()(int which, id_t who, int *value) {
	(void)which;
	(void)who;
	// Nice levels are the scheduler's (radix-scheduling(7)), not yet
	// reachable from POSIX's calls (docs/design/posix.md).
	*value = 0;
	return 0;
}

int Sysdeps<SetPriority>::operator()(int which, id_t who, int prio) {
	(void)which;
	(void)who;
	return prio < 0 ? EACCES : 0;
}

int Sysdeps<Nice>::operator()(int nice, int *new_nice) {
	if(nice < 0)
		return EPERM;
	*new_nice = 0;
	return 0;
}

int Sysdeps<Sigaction>::operator()(int signum, const struct sigaction *__restrict act,
		struct sigaction *__restrict oldact) {
	if(signum < 1 || signum > 64)
		return EINVAL;
	uint64_t handler = 0;
	uint64_t flags = 0;
	uint64_t mask = 0;
	if(act) {
		handler = reinterpret_cast<uint64_t>(act->sa_handler);
		// Every handler returns through the C library's restorer.
		flags = static_cast<uint32_t>(act->sa_flags) | SA_RESTORER;
		mask = mask_of(&act->sa_mask);
	}
	struct radix_process_signal_action_results r;
	uint64_t status = radix_process_signal_action(radix::SLOT_PROCESS,
			static_cast<uint32_t>(signum), act != nullptr, handler, flags, mask,
			reinterpret_cast<uint64_t>(&__radix_signal_restorer), &r);
	if(status)
		return radix::to_errno(status);
	if(oldact) {
		memset(oldact, 0, sizeof(*oldact));
		oldact->sa_handler = reinterpret_cast<void (*)(int)>(r.handler);
		oldact->sa_flags = static_cast<int>(r.flags);
		set_of(r.mask, &oldact->sa_mask);
		oldact->sa_restorer = reinterpret_cast<void (*)()>(r.restorer);
	}
	return 0;
}

int Sysdeps<ThreadSigmask>::operator()(int how, const sigset_t *__restrict set,
		sigset_t *__restrict retrieve) {
	enum radix_mask_how kernel_how = RADIX_MASK_HOW_QUERY;
	uint64_t mask = 0;
	if(set) {
		// POSIX's SIG_BLOCK, SIG_UNBLOCK and SIG_SETMASK are the kernel's.
		if(how < SIG_BLOCK || how > SIG_SETMASK)
			return EINVAL;
		kernel_how = static_cast<enum radix_mask_how>(how);
		mask = mask_of(set);
	}
	struct radix_process_signal_mask_results r;
	if(uint64_t status = radix_process_signal_mask(radix::SLOT_PROCESS, kernel_how, mask, &r);
			status)
		return radix::to_errno(status);
	if(retrieve)
		set_of(r.old, retrieve);
	return 0;
}

int Sysdeps<Sigprocmask>::operator()(int how, const sigset_t *__restrict set,
		sigset_t *__restrict retrieve) {
	return sysdep<ThreadSigmask>(how, set, retrieve);
}

int Sysdeps<Sigsuspend>::operator()(const sigset_t *set) {
	return radix::to_errno(radix_process_signal_suspend(radix::SLOT_PROCESS, mask_of(set)));
}

int Sysdeps<Pause>::operator()() {
	sigset_t now;
	if(int e = sysdep<ThreadSigmask>(SIG_BLOCK, nullptr, &now); e)
		return e;
	return sysdep<Sigsuspend>(&now);
}

int Sysdeps<Sigpending>::operator()(sigset_t *set) {
	struct radix_process_signal_pending_results r;
	if(uint64_t status = radix_process_signal_pending(radix::SLOT_PROCESS, &r); status)
		return radix::to_errno(status);
	set_of(r.set, set);
	return 0;
}

int Sysdeps<Sigaltstack>::operator()(const stack_t *ss, stack_t *oss) {
	struct radix_process_signal_stack_results r;
	// SS_ONSTACK and SS_DISABLE are the kernel's StackFlags.
	uint64_t status = radix_process_signal_stack(radix::SLOT_PROCESS, ss != nullptr,
			ss ? reinterpret_cast<uint64_t>(ss->ss_sp) : 0,
			ss ? static_cast<uint32_t>(ss->ss_flags) : 0, ss ? ss->ss_size : 0, &r);
	if(status)
		return radix::to_errno(status);
	if(oss) {
		oss->ss_sp = reinterpret_cast<void *>(r.address);
		oss->ss_flags = static_cast<int>(r.flags);
		oss->ss_size = r.size;
	}
	return 0;
}

int Sysdeps<Sigtimedwait>::operator()(const sigset_t *__restrict set,
		siginfo_t *__restrict info, const struct timespec *__restrict timeout, int *out_signal) {
	uint64_t timeout_ns = 0;
	if(timeout) {
		if(timeout->tv_sec < 0 || timeout->tv_nsec < 0 || timeout->tv_nsec >= 1000000000)
			return EINVAL;
		timeout_ns = static_cast<uint64_t>(timeout->tv_sec) * NS_PER_SEC
				+ static_cast<uint64_t>(timeout->tv_nsec);
	}
	struct radix_process_signal_wait_results r;
	uint64_t status = radix_process_signal_wait(radix::SLOT_PROCESS, mask_of(set), timeout_ns,
			timeout == nullptr, &r);
	if(status) {
		int e = radix::to_errno(status);
		return e == ETIMEDOUT ? EAGAIN : e;
	}
	if(info) {
		memset(info, 0, sizeof(*info));
		info->si_signo = static_cast<int>(r.signal);
		info->si_code = r.code;
		info->si_pid = static_cast<pid_t>(r.pid);
		info->si_status = static_cast<int>(r.status);
	}
	*out_signal = static_cast<int>(r.signal);
	return 0;
}

int Sysdeps<SetItimer>::operator()(int which, const struct itimerval *new_value,
		struct itimerval *old_value) {
	// ITIMER_REAL, ITIMER_VIRTUAL and ITIMER_PROF are the kernel's
	// TimerKind.
	if(which < ITIMER_REAL || which > ITIMER_PROF)
		return EINVAL;
	struct radix_process_set_timer_results r;
	uint64_t status = radix_process_set_timer(radix::SLOT_PROCESS,
			static_cast<enum radix_timer_kind>(which), ns_of(new_value->it_value),
			ns_of(new_value->it_interval), &r);
	if(status)
		return radix::to_errno(status);
	if(old_value) {
		old_value->it_value = timeval_of(r.value_ns);
		old_value->it_interval = timeval_of(r.interval_ns);
	}
	return 0;
}

int Sysdeps<GetItimer>::operator()(int which, struct itimerval *curr_value) {
	// The kernel reads a timer only by setting it: off, then back on with
	// what was left.
	struct itimerval off = {};
	if(int e = sysdep<SetItimer>(which, &off, curr_value); e)
		return e;
	if(curr_value->it_value.tv_sec || curr_value->it_value.tv_usec) {
		struct itimerval again = *curr_value;
		return sysdep<SetItimer>(which, &again, nullptr);
	}
	return 0;
}

int Sysdeps<Times>::operator()(struct tms *tms, clock_t *out) {
	struct radix_process_times_results r;
	if(uint64_t status = radix_process_times(radix::SLOT_PROCESS, &r); status)
		return radix::to_errno(status);
	// Clock ticks of 10 ms (sysconf(_SC_CLK_TCK)).
	constexpr uint64_t TICK = 10000000;
	tms->tms_utime = static_cast<clock_t>(r.user_ns / TICK);
	tms->tms_stime = static_cast<clock_t>(r.system_ns / TICK);
	tms->tms_cutime = static_cast<clock_t>(r.children_user_ns / TICK);
	tms->tms_cstime = static_cast<clock_t>(r.children_system_ns / TICK);
	time_t secs;
	long nanos;
	if(int e = sysdep<ClockGet>(CLOCK_MONOTONIC, &secs, &nanos); e)
		return e;
	*out = static_cast<clock_t>((static_cast<uint64_t>(secs) * NS_PER_SEC + nanos) / TICK);
	return 0;
}

int Sysdeps<GetRusage>::operator()(int scope, struct rusage *usage) {
	memset(usage, 0, sizeof(*usage));
	// Linux's RUSAGE_THREAD, which <sys/resource.h> has only with mlibc's
	// Linux interfaces.
	if(scope == 1) {
		time_t secs;
		long nanos;
		if(int e = sysdep<ClockGet>(CLOCK_THREAD_CPUTIME_ID, &secs, &nanos); e)
			return e;
		usage->ru_utime = timeval_of(static_cast<uint64_t>(secs) * NS_PER_SEC + nanos);
		return 0;
	}
	struct radix_process_times_results r;
	if(uint64_t status = radix_process_times(radix::SLOT_PROCESS, &r); status)
		return radix::to_errno(status);
	if(scope == RUSAGE_SELF) {
		usage->ru_utime = timeval_of(r.user_ns);
		usage->ru_stime = timeval_of(r.system_ns);
	} else if(scope == RUSAGE_CHILDREN) {
		usage->ru_utime = timeval_of(r.children_user_ns);
		usage->ru_stime = timeval_of(r.children_system_ns);
	} else {
		return EINVAL;
	}
	return 0;
}

} // namespace mlibc
