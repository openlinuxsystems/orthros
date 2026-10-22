// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Signals: handlers and what they are told, masks, pending signals,
// sigsuspend, sigtimedwait, default actions, stopping and continuing,
// faults, interrupted calls, the alternate stack, and timers.

#include <setjmp.h>
#include <stdint.h>
#include <sys/time.h>

#include "check.h"

static volatile sig_atomic_t caught;
static volatile sig_atomic_t caught_signal;
static volatile pid_t caught_pid;
static volatile int caught_code;
static volatile int caught_blocked;

static void on_info(int signal, siginfo_t *info, void *context) {
	(void)context;
	caught++;
	caught_signal = signal;
	caught_pid = info->si_pid;
	caught_code = info->si_code;
	sigset_t now;
	sigprocmask(SIG_BLOCK, NULL, &now);
	caught_blocked = sigismember(&now, signal) && sigismember(&now, SIGUSR2);
}

static void on_signal(int signal) {
	caught++;
	caught_signal = signal;
}

static void handle(int signal, void (*handler)(int), int flags) {
	struct sigaction action;
	memset(&action, 0, sizeof(action));
	action.sa_handler = handler;
	action.sa_flags = flags;
	sigemptyset(&action.sa_mask);
	CHECK(sigaction(signal, &action, NULL) == 0);
}

// A handler runs with what it was told, with the signal and sa_mask
// blocked, and the mask comes back after.
static void handlers(void) {
	struct sigaction action;
	memset(&action, 0, sizeof(action));
	action.sa_sigaction = on_info;
	action.sa_flags = SA_SIGINFO;
	sigemptyset(&action.sa_mask);
	sigaddset(&action.sa_mask, SIGUSR2);
	CHECK(sigaction(SIGUSR1, &action, NULL) == 0);
	struct sigaction old;
	CHECK(sigaction(SIGUSR1, NULL, &old) == 0 && old.sa_sigaction == on_info);
	CHECK(old.sa_flags & SA_SIGINFO);
	caught = 0;
	CHECK(kill(getpid(), SIGUSR1) == 0);
	CHECK(caught == 1 && caught_signal == SIGUSR1);
	CHECK(caught_pid == getpid() && caught_code == SI_USER);
	CHECK(caught_blocked);
	sigset_t now;
	CHECK(sigprocmask(SIG_BLOCK, NULL, &now) == 0);
	CHECK(!sigismember(&now, SIGUSR1) && !sigismember(&now, SIGUSR2));
	CHECK(raise(SIGUSR1) == 0);
	CHECK(caught == 2);
	// A signal from another process says who sent it.
	pid_t parent = getpid();
	pid_t pid = check_fork();
	if(pid == 0) {
		kill(parent, SIGUSR1);
		_exit(0);
	}
	CHECK_CHILD(pid);
	// Delivered by now, or at the latest when the next call returns.
	for(int i = 0; i < 100 && caught < 3; i++)
		check_sleep_ms(1);
	CHECK(caught == 3 && caught_pid == pid && caught_code == SI_USER);
	// Signal 0 and bad signals.
	CHECK(kill(getpid(), 0) == 0);
	CHECK_ERROR(kill(getpid(), 65), EINVAL);
	CHECK_ERROR(kill(999999, SIGUSR1), ESRCH);
	CHECK_ERROR(sigaction(SIGKILL, &action, NULL), EINVAL);
	CHECK_ERROR(sigaction(SIGSTOP, &action, NULL), EINVAL);
	signal(SIGUSR1, SIG_DFL);
}

// A blocked signal stays pending until it is unblocked.
static void masks(void) {
	handle(SIGUSR1, on_signal, 0);
	sigset_t block, old;
	sigemptyset(&block);
	sigaddset(&block, SIGUSR1);
	CHECK(sigprocmask(SIG_BLOCK, &block, &old) == 0);
	caught = 0;
	raise(SIGUSR1);
	raise(SIGUSR1);
	CHECK(caught == 0);
	sigset_t pending;
	CHECK(sigpending(&pending) == 0 && sigismember(&pending, SIGUSR1));
	// Ordinary signals do not queue: two make one.
	CHECK(sigprocmask(SIG_SETMASK, &old, NULL) == 0);
	CHECK(caught == 1);
	CHECK(sigpending(&pending) == 0 && !sigismember(&pending, SIGUSR1));
	// SIGKILL and SIGSTOP cannot be blocked.
	sigfillset(&block);
	CHECK(sigprocmask(SIG_SETMASK, &block, &old) == 0);
	sigset_t now;
	CHECK(sigprocmask(SIG_SETMASK, &old, &now) == 0);
	CHECK(!sigismember(&now, SIGKILL) && !sigismember(&now, SIGSTOP));
	CHECK(sigismember(&now, SIGUSR1));
	// An ignored signal is dropped, even when blocked and then ignored.
	signal(SIGUSR2, SIG_IGN);
	raise(SIGUSR2);
	CHECK(sigpending(&pending) == 0 && !sigismember(&pending, SIGUSR2));
	signal(SIGUSR2, SIG_DFL);
	signal(SIGUSR1, SIG_DFL);
}

// sigsuspend waits with another mask until a handler has run.
static void suspends(void) {
	handle(SIGUSR1, on_signal, 0);
	sigset_t block, old;
	sigemptyset(&block);
	sigaddset(&block, SIGUSR1);
	sigprocmask(SIG_BLOCK, &block, &old);
	caught = 0;
	pid_t parent = getpid();
	pid_t pid = check_fork();
	if(pid == 0) {
		check_sleep_ms(20);
		kill(parent, SIGUSR1);
		_exit(0);
	}
	sigset_t wait_mask;
	sigemptyset(&wait_mask);
	CHECK_ERROR(sigsuspend(&wait_mask), EINTR);
	CHECK(caught == 1);
	// The mask is the old one again: SIGUSR1 blocked.
	sigset_t now;
	sigprocmask(SIG_BLOCK, NULL, &now);
	CHECK(sigismember(&now, SIGUSR1));
	CHECK_CHILD(pid);
	// sigtimedwait takes a blocked signal without a handler running.
	raise(SIGUSR1);
	siginfo_t info;
	struct timespec timeout = {1, 0};
	CHECK(sigtimedwait(&block, &info, &timeout) == SIGUSR1);
	CHECK(info.si_signo == SIGUSR1 && caught == 1);
	struct timespec short_wait = {0, 20000000};
	long long start = check_now_ns();
	CHECK_ERROR(sigtimedwait(&block, &info, &short_wait), EAGAIN);
	CHECK(check_now_ns() - start >= 15000000);
	int got;
	raise(SIGUSR1);
	CHECK(sigwait(&block, &got) == 0 && got == SIGUSR1);
	sigprocmask(SIG_SETMASK, &old, NULL);
	signal(SIGUSR1, SIG_DFL);
}

// What a signal's default action does to a child.
static void defaults(void) {
	static const int ends[] = {SIGTERM, SIGINT, SIGHUP, SIGUSR1, SIGKILL, SIGALRM};
	for(size_t i = 0; i < sizeof(ends) / sizeof(ends[0]); i++) {
		pid_t pid = check_fork();
		if(pid == 0) {
			raise(ends[i]);
			_exit(0);
		}
		int status = check_wait(pid);
		CHECK(WIFSIGNALED(status) && WTERMSIG(status) == ends[i]);
	}
	// abort() ends with SIGABRT even with a handler that returns.
	pid_t pid = check_fork();
	if(pid == 0) {
		handle(SIGABRT, on_signal, 0);
		abort();
	}
	int status = check_wait(pid);
	CHECK(WIFSIGNALED(status) && WTERMSIG(status) == SIGABRT);
	// SIGCHLD and SIGURG do nothing.
	signal(SIGCHLD, SIG_DFL);
	raise(SIGCHLD);
	raise(SIGURG);
}

static volatile sig_atomic_t children_ended;

static void on_child(int signal) {
	(void)signal;
	children_ended++;
}

// SIGCHLD comes when a child ends, and stops and continues.
static void children(void) {
	children_ended = 0;
	handle(SIGCHLD, on_child, SA_RESTART);
	pid_t pid = check_fork();
	if(pid == 0)
		_exit(0);
	CHECK_CHILD(pid);
	for(int i = 0; i < 100 && !children_ended; i++)
		check_sleep_ms(1);
	CHECK(children_ended == 1);
	// A stopped child, and one that continued.
	int fds[2];
	CHECK(pipe(fds) == 0);
	pid = check_fork();
	if(pid == 0) {
		close(fds[1]);
		char byte;
		while(read(fds[0], &byte, 1) < 0 && errno == EINTR) {
		}
		_exit(9);
	}
	close(fds[0]);
	CHECK(kill(pid, SIGSTOP) == 0);
	int status;
	CHECK(waitpid(pid, &status, WUNTRACED) == pid);
	CHECK(WIFSTOPPED(status) && WSTOPSIG(status) == SIGSTOP);
	CHECK(kill(pid, SIGCONT) == 0);
	CHECK(waitpid(pid, &status, WCONTINUED) == pid);
	CHECK(WIFCONTINUED(status));
	CHECK(write(fds[1], "x", 1) == 1);
	close(fds[1]);
	status = check_wait(pid);
	CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 9);
	// With SIGCHLD ignored, children are reaped as they end.
	signal(SIGCHLD, SIG_IGN);
	pid = check_fork();
	if(pid == 0)
		_exit(0);
	check_sleep_ms(20);
	CHECK_ERROR(waitpid(pid, NULL, 0), ECHILD);
	signal(SIGCHLD, SIG_DFL);
}

static sigjmp_buf fault_jump;
static void *volatile fault_address;

static void on_fault(int signal, siginfo_t *info, void *context) {
	(void)context;
	caught_signal = signal;
	fault_address = info->si_addr;
	siglongjmp(fault_jump, 1);
}

// A fault is a signal: one that ends the process, or that a handler
// takes and jumps out of.
static void faults(void) {
	pid_t pid = check_fork();
	if(pid == 0) {
		*(volatile int *)16 = 1;
		_exit(0);
	}
	int status = check_wait(pid);
	CHECK(WIFSIGNALED(status) && WTERMSIG(status) == SIGSEGV);
	pid = check_fork();
	if(pid == 0) {
		__builtin_trap();
	}
	status = check_wait(pid);
	CHECK(WIFSIGNALED(status) && WTERMSIG(status) == SIGILL);
	pid = check_fork();
	if(pid == 0) {
		// In assembly: C's division by zero is undefined, so a compiler
		// may leave the division out.
		unsigned int quotient, zero = 0;
		__asm__ volatile("xorl %%edx, %%edx\n\tdivl %1" : "=a"(quotient) : "r"(zero), "a"(1u) : "edx");
		_exit((int)quotient);
	}
	status = check_wait(pid);
	CHECK(WIFSIGNALED(status) && WTERMSIG(status) == SIGFPE);
	struct sigaction action;
	memset(&action, 0, sizeof(action));
	action.sa_sigaction = on_fault;
	action.sa_flags = SA_SIGINFO;
	sigemptyset(&action.sa_mask);
	CHECK(sigaction(SIGSEGV, &action, NULL) == 0);
	caught_signal = 0;
	if(!sigsetjmp(fault_jump, 1))
		*(volatile int *)0x1234 = 1;
	CHECK(caught_signal == SIGSEGV && fault_address == (void *)0x1234);
	// Again: siglongjmp put the mask back, so SIGSEGV is not blocked.
	caught_signal = 0;
	if(!sigsetjmp(fault_jump, 1))
		*(volatile int *)0x5678 = 1;
	CHECK(caught_signal == SIGSEGV && fault_address == (void *)0x5678);
	signal(SIGSEGV, SIG_DFL);
}

// A blocking call a handler interrupts fails with EINTR, unless the
// handler has SA_RESTART.
static void interrupts(void) {
	int fds[2];
	CHECK(pipe(fds) == 0);
	handle(SIGALRM, on_signal, 0);
	struct itimerval timer = {{0, 0}, {0, 20000}};
	CHECK(setitimer(ITIMER_REAL, &timer, NULL) == 0);
	char byte;
	CHECK_ERROR(read(fds[0], &byte, 1), EINTR);
	// With SA_RESTART the read goes on, until a child writes.
	handle(SIGALRM, on_signal, SA_RESTART);
	caught = 0;
	CHECK(setitimer(ITIMER_REAL, &timer, NULL) == 0);
	pid_t pid = check_fork();
	if(pid == 0) {
		check_sleep_ms(60);
		write(fds[1], "y", 1);
		_exit(0);
	}
	CHECK(read(fds[0], &byte, 1) == 1 && byte == 'y');
	CHECK(caught == 1);
	CHECK_CHILD(pid);
	// pause() returns once a handler has run.
	alarm(1);
	struct itimerval left;
	CHECK(getitimer(ITIMER_REAL, &left) == 0 && left.it_value.tv_sec <= 1);
	CHECK(left.it_value.tv_sec > 0 || left.it_value.tv_usec > 0);
	alarm(0);
	CHECK(setitimer(ITIMER_REAL, &timer, NULL) == 0);
	CHECK_ERROR(pause(), EINTR);
	// nanosleep says how much was left.
	CHECK(setitimer(ITIMER_REAL, &timer, NULL) == 0);
	struct timespec sleep_for = {2, 0}, remaining = {0, 0};
	CHECK_ERROR(nanosleep(&sleep_for, &remaining), EINTR);
	CHECK(remaining.tv_sec >= 1 && remaining.tv_sec <= 2);
	signal(SIGALRM, SIG_DFL);
	close(fds[0]);
	close(fds[1]);
}

static char alternate[65536];
static volatile int on_alternate;

static void on_stack(int signal) {
	(void)signal;
	char here;
	on_alternate = &here >= alternate && &here < alternate + sizeof(alternate);
	stack_t now;
	sigaltstack(NULL, &now);
	on_alternate = on_alternate && (now.ss_flags & SS_ONSTACK);
}

// A handler with SA_ONSTACK runs on the alternate stack.
static void stacks(void) {
	stack_t stack = {.ss_sp = alternate, .ss_size = sizeof(alternate), .ss_flags = 0};
	CHECK(sigaltstack(&stack, NULL) == 0);
	stack_t now;
	CHECK(sigaltstack(NULL, &now) == 0 && now.ss_sp == alternate && !(now.ss_flags & SS_ONSTACK));
	handle(SIGUSR1, on_stack, SA_ONSTACK);
	on_alternate = 0;
	raise(SIGUSR1);
	CHECK(on_alternate);
	stack.ss_flags = SS_DISABLE;
	CHECK(sigaltstack(&stack, NULL) == 0);
	stack.ss_size = 512;
	stack.ss_flags = 0;
	CHECK_ERROR(sigaltstack(&stack, NULL), ENOMEM);
	signal(SIGUSR1, SIG_DFL);
}

static volatile double fp_seen;

static void on_float(int signal) {
	(void)signal;
	volatile double x = 3.0;
	for(int i = 0; i < 10; i++)
		x = x * 1.5 + 0.25;
	fp_seen = x;
}

// The floating-point registers a handler uses come back as they were.
static void floats(void) {
	handle(SIGUSR1, on_float, 0);
	double a = 1.0, b = 0.0;
	for(int i = 0; i < 1000; i++) {
		b += a / 3.0;
		if(i == 500)
			raise(SIGUSR1);
	}
	CHECK(b > 333.33 && b < 333.34);
	CHECK(fp_seen > 100.0);
	signal(SIGUSR1, SIG_DFL);
}

static volatile int order[4];
static volatile int order_count;

static void on_second(int signal) {
	order[order_count++] = signal;
}

static void on_first(int signal) {
	order[order_count++] = signal;
	raise(SIGUSR2);
	order[order_count++] = -signal;
}

// A handler can take another signal while it runs.
static void nested(void) {
	handle(SIGUSR1, on_first, 0);
	handle(SIGUSR2, on_second, 0);
	order_count = 0;
	raise(SIGUSR1);
	CHECK(order_count == 3 && order[0] == SIGUSR1 && order[1] == SIGUSR2 && order[2] == -SIGUSR1);
	signal(SIGUSR1, SIG_DFL);
	signal(SIGUSR2, SIG_DFL);
}

int main(void) {
	check_start("signal");
	RUN(handlers);
	RUN(masks);
	RUN(suspends);
	RUN(defaults);
	RUN(children);
	RUN(faults);
	RUN(interrupts);
	RUN(stacks);
	RUN(floats);
	RUN(nested);
	return check_end();
}
