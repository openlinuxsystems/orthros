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
#include <mlibc/sigset.hpp>
#include <mlibc/tcb.hpp>
#include <pthread.h>
#include <sched.h>
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

int Sysdeps<Waitid>::operator()(idtype_t idtype, id_t id, siginfo_t *info, int options) {
	pid_t pid;
	switch(idtype) {
	case P_ALL:
		pid = -1;
		break;
	case P_PID:
		pid = static_cast<pid_t>(id);
		break;
	case P_PGID:
		// Group 0 is the caller's, as waitpid's 0.
		pid = -static_cast<pid_t>(id);
		break;
	default:
		return EINVAL;
	}
	// The kernel always reports children that ended: waitid without
	// WEXITED is not supported (docs/design/posix.md).
	if(!(options & WEXITED) || (options & ~(WEXITED | WSTOPPED | WCONTINUED | WNOHANG | WNOWAIT)))
		return EINVAL;
	int status;
	pid_t found;
	int flags = options & (WNOHANG | WCONTINUED | WNOWAIT);
	if(options & WSTOPPED)
		flags |= WUNTRACED;
	if(int e = sysdep<Waitpid>(pid, &status, flags, nullptr, &found); e)
		return e;
	memset(info, 0, sizeof(*info));
	if(!found)
		return 0;
	info->si_signo = SIGCHLD;
	info->si_pid = found;
	if(WIFEXITED(status)) {
		info->si_code = CLD_EXITED;
		info->si_status = WEXITSTATUS(status);
	} else if(WIFSIGNALED(status)) {
		info->si_code = WCOREDUMP(status) ? CLD_DUMPED : CLD_KILLED;
		info->si_status = WTERMSIG(status);
	} else if(WIFSTOPPED(status)) {
		info->si_code = CLD_STOPPED;
		info->si_status = WSTOPSIG(status);
	} else {
		info->si_code = CLD_CONTINUED;
		info->si_status = SIGCONT;
	}
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
	if(sig < 0 || sig > 64)
		return EINVAL;
	uint64_t value = 0;
	memcpy(&value, &val, sizeof(val));
	return radix::to_errno(radix_process_signal_queue(radix::SLOT_PROCESS,
			static_cast<uint64_t>(static_cast<int64_t>(pid)), static_cast<uint32_t>(sig), value));
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
	// PRIO_PROCESS, PRIO_PGRP and PRIO_USER are the kernel's
	// PriorityWhich; as on Linux, a process's number names its first
	// thread (radix-posix(7)).
	struct radix_process_priority_results r;
	if(uint64_t status = radix_process_priority(radix::SLOT_PROCESS,
			static_cast<enum radix_priority_which>(which), who, false, 0, &r); status)
		return radix::to_errno(status);
	*value = r.nice;
	return 0;
}

int Sysdeps<SetPriority>::operator()(int which, id_t who, int prio) {
	struct radix_process_priority_results r;
	return radix::to_errno(radix_process_priority(radix::SLOT_PROCESS,
			static_cast<enum radix_priority_which>(which), who, true, prio, &r));
}

int Sysdeps<Nice>::operator()(int nice, int *new_nice) {
	int now;
	if(int e = sysdep<GetPriority>(PRIO_PROCESS, 0, &now); e)
		return e;
	// The kernel clamps the level to -20 to 19.
	long wanted = static_cast<long>(now) + nice;
	if(wanted < -20)
		wanted = -20;
	if(wanted > 19)
		wanted = 19;
	if(int e = sysdep<SetPriority>(PRIO_PROCESS, 0, static_cast<int>(wanted)); e)
		// POSIX's nice fails with EPERM where setpriority has EACCES.
		return e == EACCES ? EPERM : e;
	return sysdep<GetPriority>(PRIO_PROCESS, 0, new_nice);
}

namespace {

// Reads, and with `set` changes, the policy and priority of thread
// `thread` (0: the calling one): radix-process(2)'s `scheduling`, with
// POSIX's policies, which are the kernel's (with Linux's numbers).
int scheduling(uint64_t thread, bool set, int *policy, int *priority) {
	if(set && (*policy < 0 || *priority < 0))
		return EINVAL;
	struct radix_process_scheduling_results r;
	if(uint64_t status = radix_process_scheduling(radix::SLOT_PROCESS, thread, set,
			static_cast<enum radix_scheduling_policy>(set ? *policy : 0),
			set ? static_cast<uint32_t>(*priority) : 0, &r); status)
		return radix::to_errno(status);
	if(!set) {
		*policy = static_cast<int>(r.policy);
		*priority = static_cast<int>(r.priority);
	}
	return 0;
}

// The thread a process number names for the sched_ calls: 0 the calling
// one; or else the process's first thread, which has its number.
int thread_of(pid_t pid, uint64_t *thread) {
	if(pid < 0)
		return EINVAL;
	*thread = static_cast<uint64_t>(pid);
	return 0;
}

} // namespace

int Sysdeps<GetScheduler>::operator()(pid_t pid, int *policy) {
	uint64_t thread;
	if(int e = thread_of(pid, &thread); e)
		return e;
	int priority;
	return scheduling(thread, false, policy, &priority);
}

int Sysdeps<SetScheduler>::operator()(pid_t pid, int policy, const struct sched_param *param) {
	uint64_t thread;
	if(int e = thread_of(pid, &thread); e)
		return e;
	if(!param)
		return EINVAL;
	int priority = param->sched_priority;
	return scheduling(thread, true, &policy, &priority);
}

int Sysdeps<GetParam>::operator()(pid_t pid, struct sched_param *param) {
	uint64_t thread;
	if(int e = thread_of(pid, &thread); e)
		return e;
	int policy;
	return scheduling(thread, false, &policy, &param->sched_priority);
}

int Sysdeps<SetParam>::operator()(pid_t pid, const struct sched_param *param) {
	uint64_t thread;
	if(int e = thread_of(pid, &thread); e)
		return e;
	if(!param)
		return EINVAL;
	int policy;
	int priority;
	if(int e = scheduling(thread, false, &policy, &priority); e)
		return e;
	priority = param->sched_priority;
	return scheduling(thread, true, &policy, &priority);
}

int Sysdeps<GetSchedparam>::operator()(void *tcb, int *policy, struct sched_param *param) {
	auto thread = static_cast<uint64_t>(reinterpret_cast<Tcb *>(tcb)->tid);
	return scheduling(thread, false, policy, &param->sched_priority);
}

int Sysdeps<SetSchedparam>::operator()(void *tcb, int policy, const struct sched_param *param) {
	auto thread = static_cast<uint64_t>(reinterpret_cast<Tcb *>(tcb)->tid);
	int priority = param->sched_priority;
	return scheduling(thread, true, &policy, &priority);
}

int Sysdeps<ThreadSetname>::operator()(void *tcb, const char *name) {
	auto thread = static_cast<uint64_t>(reinterpret_cast<Tcb *>(tcb)->tid);
	// Linux's limit of 15 bytes, which the kernel shares.
	size_t length = strlen(name);
	if(length > RADIX_PROCESS_NAME_MAX)
		return ERANGE;
	return radix::to_errno(radix_process_set_thread_name(radix::SLOT_PROCESS, thread, name, length));
}

int Sysdeps<ThreadGetname>::operator()(void *tcb, char *name, size_t size) {
	auto thread = static_cast<uint64_t>(reinterpret_cast<Tcb *>(tcb)->tid);
	if(!size)
		return ERANGE;
	struct radix_process_thread_name_results r;
	// One byte is kept for the NUL.
	if(uint64_t status = radix_process_thread_name(radix::SLOT_PROCESS, thread, name, size - 1, &r);
			status)
		return radix::to_errno(status);
	name[r.length] = 0;
	return 0;
}

int Sysdeps<GetMaxPriority>::operator()(int policy, int *out) {
	switch(policy) {
	case SCHED_FIFO:
	case SCHED_RR:
		*out = 99;
		return 0;
	case SCHED_OTHER:
	case SCHED_BATCH:
	case SCHED_IDLE:
		*out = 0;
		return 0;
	default:
		return EINVAL;
	}
}

int Sysdeps<GetMinPriority>::operator()(int policy, int *out) {
	switch(policy) {
	case SCHED_FIFO:
	case SCHED_RR:
		*out = 1;
		return 0;
	case SCHED_OTHER:
	case SCHED_BATCH:
	case SCHED_IDLE:
		*out = 0;
		return 0;
	default:
		return EINVAL;
	}
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
		// As the kernel lays a SignalInfo out for a handler
		// (radix-signal(2)): the sender's number (or a timer's number and
		// overrun) at 16, the status or value at 24.
		memset(info, 0, sizeof(*info));
		info->si_signo = static_cast<int>(r.signal);
		info->si_code = r.code;
		memcpy(reinterpret_cast<char *>(info) + 16, &r.pid, sizeof(r.pid));
		memcpy(reinterpret_cast<char *>(info) + 24, &r.status, sizeof(r.status));
	}
	*out_signal = static_cast<int>(r.signal);
	return 0;
}

namespace {

// Nanoseconds in `ts`, or EINVAL if it is not a time.
int timespec_ns(const struct timespec &ts, uint64_t *ns) {
	if(ts.tv_sec < 0 || ts.tv_nsec < 0 || ts.tv_nsec >= static_cast<long>(NS_PER_SEC))
		return EINVAL;
	*ns = static_cast<uint64_t>(ts.tv_sec) * NS_PER_SEC + static_cast<uint64_t>(ts.tv_nsec);
	return 0;
}

struct timespec timespec_of(uint64_t ns) {
	struct timespec ts;
	ts.tv_sec = static_cast<time_t>(ns / NS_PER_SEC);
	ts.tv_nsec = static_cast<long>(ns % NS_PER_SEC);
	return ts;
}

// A SIGEV_THREAD timer: a thread of the C library's own waits for the
// timer's signal, SIGTIMER, which no program sees, and calls the
// timer's function for each expiry.
struct TimerThread {
	void (*function)(union sigval);
	union sigval value;
	// Its number.
	pid_t thread;
	// Set by timer_delete: the thread ends.
	int deleted;
};

// The threads of the SIGEV_THREAD timers, by timer number.
TimerThread *timer_threads[RADIX_PROCESS_TIMERS_MAX];

void *run_timer_thread(void *argument) {
	auto timer = static_cast<TimerThread *>(argument);
	sigset_t set;
	sigemptyset(&set);
	mlibc::sigaddset_internal(&set, SIGTIMER);
	for(;;) {
		siginfo_t info;
		int signal;
		if(sysdep<Sigtimedwait>(&set, &info, nullptr, &signal))
			continue;
		if(__atomic_load_n(&timer->deleted, __ATOMIC_ACQUIRE))
			break;
		if(info.si_code == SI_TIMER)
			timer->function(timer->value);
	}
	free(timer);
	return nullptr;
}

// Ends the thread of `timer`, which timer_delete deleted or
// timer_create could not make. The thread frees `timer`.
void end_timer_thread(TimerThread *timer) {
	pid_t thread = timer->thread;
	__atomic_store_n(&timer->deleted, 1, __ATOMIC_RELEASE);
	sysdep<Tgkill>(sysdep<GetPid>(), thread, SIGTIMER);
}

// Starts the thread of a SIGEV_THREAD timer `event` asks for; returns its
// number in `thread`.
int start_timer_thread(const struct sigevent *event, TimerThread **made, pid_t *thread) {
	auto timer = static_cast<TimerThread *>(calloc(1, sizeof(TimerThread)));
	if(!timer)
		return EAGAIN;
	timer->function = event->sigev_notify_function;
	timer->value = event->sigev_value;
	pthread_attr_t attr;
	if(event->sigev_notify_attributes)
		attr = *event->sigev_notify_attributes;
	else
		pthread_attr_init(&attr);
	pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED);
	// It starts with every signal blocked: it takes SIGTIMER alone, and
	// only by waiting for it. sigfillset leaves the C library's own
	// signals out, so the set is filled by hand.
	sigset_t all;
	sigset_t old;
	memset(&all, 0xff, sizeof(all));
	sysdep<ThreadSigmask>(SIG_BLOCK, &all, &old);
	pthread_t handle;
	int e = pthread_create(&handle, &attr, run_timer_thread, timer);
	sysdep<ThreadSigmask>(SIG_SETMASK, &old, nullptr);
	if(!event->sigev_notify_attributes)
		pthread_attr_destroy(&attr);
	if(e) {
		free(timer);
		return e;
	}
	timer->thread = reinterpret_cast<Tcb *>(handle)->tid;
	*made = timer;
	*thread = timer->thread;
	return 0;
}

// The number of timer `t`, or EINVAL if it cannot be one.
int timer_number(timer_t t, uint32_t *number) {
	auto n = reinterpret_cast<uintptr_t>(t);
	if(n >= RADIX_PROCESS_TIMERS_MAX)
		return EINVAL;
	*number = static_cast<uint32_t>(n);
	return 0;
}

} // namespace

int Sysdeps<TimerCreate>::operator()(clockid_t clk, struct sigevent *__restrict evp,
		timer_t *__restrict res) {
	// The kernel's ClockKind numbers the clocks as Linux does.
	if(clk < 0)
		return EINVAL;
	// Without a sigevent, SIGALRM carrying the timer's number.
	enum radix_timer_notify notify = RADIX_TIMER_NOTIFY_ALARM;
	uint32_t signal = 0;
	uint64_t value = 0;
	pid_t thread = 0;
	TimerThread *helper = nullptr;
	if(evp) {
		signal = static_cast<uint32_t>(evp->sigev_signo);
		memcpy(&value, &evp->sigev_value, sizeof(evp->sigev_value));
		switch(evp->sigev_notify) {
		case SIGEV_SIGNAL:
			notify = RADIX_TIMER_NOTIFY_SIGNAL;
			break;
		case SIGEV_NONE:
			notify = RADIX_TIMER_NOTIFY_NONE;
			break;
		case SIGEV_THREAD_ID:
			notify = RADIX_TIMER_NOTIFY_THREAD;
			thread = evp->sigev_notify_thread_id;
			break;
		case SIGEV_THREAD:
			if(int e = start_timer_thread(evp, &helper, &thread); e)
				return e;
			notify = RADIX_TIMER_NOTIFY_THREAD;
			signal = SIGTIMER;
			value = 0;
			break;
		default:
			return EINVAL;
		}
	}
	struct radix_process_timer_create_results r;
	uint64_t status = radix_process_timer_create(radix::SLOT_PROCESS,
			static_cast<enum radix_clock_kind>(clk), notify, signal, value,
			static_cast<uint64_t>(thread), &r);
	if(status) {
		if(helper)
			end_timer_thread(helper);
		return radix::to_errno(status);
	}
	if(helper)
		__atomic_store_n(&timer_threads[r.timer], helper, __ATOMIC_RELEASE);
	*res = reinterpret_cast<timer_t>(static_cast<uintptr_t>(r.timer));
	return 0;
}

int Sysdeps<TimerSettime>::operator()(timer_t t, int flags,
		const struct itimerspec *__restrict val, struct itimerspec *__restrict old) {
	uint32_t number;
	uint64_t value_ns;
	uint64_t interval_ns;
	if(int e = timer_number(t, &number); e)
		return e;
	if(int e = timespec_ns(val->it_value, &value_ns); e)
		return e;
	if(int e = timespec_ns(val->it_interval, &interval_ns); e)
		return e;
	struct radix_process_timer_set_results r;
	if(uint64_t status = radix_process_timer_set(radix::SLOT_PROCESS, number,
			(flags & TIMER_ABSTIME) != 0, value_ns, interval_ns, &r); status)
		return radix::to_errno(status);
	if(old) {
		old->it_value = timespec_of(r.value_ns);
		old->it_interval = timespec_of(r.interval_ns);
	}
	return 0;
}

int Sysdeps<TimerGettime>::operator()(timer_t t, struct itimerspec *val) {
	uint32_t number;
	if(int e = timer_number(t, &number); e)
		return e;
	struct radix_process_timer_get_results r;
	if(uint64_t status = radix_process_timer_get(radix::SLOT_PROCESS, number, &r); status)
		return radix::to_errno(status);
	val->it_value = timespec_of(r.value_ns);
	val->it_interval = timespec_of(r.interval_ns);
	return 0;
}

int Sysdeps<TimerGetoverrun>::operator()(timer_t t, int *out) {
	uint32_t number;
	if(int e = timer_number(t, &number); e)
		return e;
	struct radix_process_timer_get_results r;
	if(uint64_t status = radix_process_timer_get(radix::SLOT_PROCESS, number, &r); status)
		return radix::to_errno(status);
	*out = static_cast<int>(r.overrun);
	return 0;
}

int Sysdeps<TimerDelete>::operator()(timer_t t) {
	uint32_t number;
	if(int e = timer_number(t, &number); e)
		return e;
	if(uint64_t status = radix_process_timer_delete(radix::SLOT_PROCESS, number); status)
		return radix::to_errno(status);
	if(auto helper = __atomic_exchange_n(&timer_threads[number], nullptr, __ATOMIC_ACQ_REL))
		end_timer_thread(helper);
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
