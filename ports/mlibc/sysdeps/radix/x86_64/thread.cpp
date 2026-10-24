// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Threads on x86-64: a new thread's stack, with a guard below it, and the
// C++ side of its entry (thread-entry.S).

#include <bits/ensure.h>
#include <errno.h>
#include <mlibc/all-sysdeps.hpp>
#include <mlibc/tcb.hpp>
#include <mlibc/threads.hpp>
#include <stdint.h>
#include <sys/mman.h>

extern "C" void __mlibc_thread_trampoline(void *(*fn)(void *), Tcb *tcb, void *arg) {
	if(mlibc::sysdep<TcbSet>(tcb))
		__ensure(!"failed to set the new thread's TCB");

	// pthread_create writes the thread's number once start returns it.
	while(__atomic_load_n(&tcb->tid, __ATOMIC_RELAXED) == 0)
		mlibc::sysdep<FutexWait>(&tcb->tid, 0, nullptr);

	// Cancellation works once the TCB is up.
	__atomic_fetch_or(&tcb->cancelBits, tcbCancelEnableBit, __ATOMIC_RELAXED);

	tcb->invokeThreadFunc(reinterpret_cast<void *>(fn), arg);

	mlibc::thread_exit(tcb->returnValue);
}

namespace {

// 2 MiB, as glibc's default on x86-64 Linux for an unlimited stack.
constexpr size_t DEFAULT_STACK = 0x200000;

constexpr size_t PAGE = 0x1000;

// `size` rounded up to whole pages, or 0 if it cannot be.
size_t page_round(size_t size) {
	if(size > SIZE_MAX - (PAGE - 1))
		return 0;
	return (size + PAGE - 1) & ~(PAGE - 1);
}

} // namespace

namespace mlibc {

int Sysdeps<PrepareStack>::operator()(void **stack, void *entry, void *arg, void *tcb,
		size_t *stack_size, size_t *guard_size, void **stack_base) {
	if(!*stack_size)
		*stack_size = DEFAULT_STACK;
	void *map;
	uintptr_t top;
	if(*stack) {
		// The caller's own stack: no guard, and it starts at its top as
		// far down as the ABI's 16 bytes ask.
		map = *stack;
		*guard_size = 0;
		top = (reinterpret_cast<uintptr_t>(map) + *stack_size) & ~uintptr_t(15);
	} else {
		// The guard first, which cannot be touched, and the stack above it,
		// both in whole pages as glibc makes them, so the stack's top is
		// on 16 bytes as the ABI asks, and the guard can be protected.
		size_t size = page_round(*stack_size);
		size_t guard = page_round(*guard_size);
		if(!size || (*guard_size && !guard) || guard > SIZE_MAX - size)
			return EAGAIN;
		*stack_size = size;
		*guard_size = guard;
		if(sysdep<VmMap>(nullptr, *stack_size + *guard_size, PROT_NONE,
				   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0, &map))
			return EAGAIN;
		if(sysdep<VmProtect>(static_cast<char *>(map) + *guard_size, *stack_size,
				   PROT_READ | PROT_WRITE)) {
			sysdep<VmUnmap>(map, *stack_size + *guard_size);
			return EAGAIN;
		}
		top = reinterpret_cast<uintptr_t>(map) + *guard_size + *stack_size;
	}
	*stack_base = map;
	// What thread-entry.S pops.
	auto sp = reinterpret_cast<uintptr_t *>(top);
	*--sp = reinterpret_cast<uintptr_t>(arg);
	*--sp = reinterpret_cast<uintptr_t>(tcb);
	*--sp = reinterpret_cast<uintptr_t>(entry);
	*stack = sp;
	return 0;
}

} // namespace mlibc
