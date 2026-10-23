#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds libc-test's functional and regression tests that apply to Radix
# into one static program, /system/tests/libc-test/run, with
# ports/mlibc-tests' runner (run.c): each test's main is renamed after it
# (regression-malloc-0 for src/regression/malloc-0.c), and each runs in
# a process of its own. `ob` runs it (ob(1), PORTS) with CC, CFLAGS,
# OBJCOPY, STRIP, PORT_DIR, PORT_SRC, PORT_BUILD and SYSROOT set.
#
# Left out, with why:
#   src/math/*: their test vectors from crlibm are GPL-2.0, which the
#     image does not take (ADR 0021)
#   *_dso: libraries for the tests that open them, not tests
#   functional/dlopen, tls_init_dlopen, tls_align_dlopen,
#     regression/tls_get_new-dtv: they open shared libraries built from
#     the _dso sources, and run is static; the dynamic test checks
#     dynamic linking instead (ADR 0071)
#   functional/crypt: crypt(3) comes with the user database (Phase 13)
#   functional/iconv_open, regression/iconv-roundtrips: iconv, which
#     comes with the locale data (Phase 12)
#   functional/ipc_msg, ipc_sem, ipc_shm: System V IPC, which Radix does
#     not have (radix-posix(7))
#   functional/socket, regression/dn_expand-empty, dn_expand-ptr-0:
#     networking and the resolver (Phase 12)
#   regression/syscall-sign-extend: Linux's syscall(2), which the C
#     library leaves out on Radix (ADR 0068)
#   functional/pthread_cancel, pthread_cancel-points,
#     regression/pthread_cancel-sem_wait, pthread_cond_wait-cancel_ignored:
#     thread cancellation, which mlibc leaves out of its cross builds
#   functional/pthread_robust, regression/pthread-robust-detach: robust
#     mutexes, and functional/pthread_mutex_pi: priority-inheritance
#     mutexes, neither of which mlibc has
#   functional/sem_open, regression/sem_close-unmap: named semaphores,
#     which mlibc does not have
#   regression/flockfile-list: it replaces malloc, which a static program
#     cannot do with mlibc
#   regression/malloc-brk-fail: it fills the address space but for a
#     64 KiB gap and expects malloc to fit there; mlibc's allocator maps
#     256 KiB at a time
#   regression/pthread_atfork-errno-clobber: it needs fork to fail under
#     an RLIMIT_NPROC of 0, which Radix keeps for programs to read and
#     Linux does not enforce for root, the only user until Phase 13
set -eu

# As libc-test's config.mak.def compiles them.
flags="-std=c99 -D_POSIX_C_SOURCE=200809L -Wall -Wno-unused-function -Wno-missing-braces"
flags="$flags -Wno-unused -Wno-overflow -Wno-unknown-pragmas -fno-builtin -frounding-math"
flags="$flags -Werror=implicit-function-declaration -Werror=implicit-int"
flags="$flags -Werror=pointer-sign -Werror=pointer-arith -I$PORT_SRC/src/common"
# Clang's own warnings about what the tests do on purpose: constants out
# of a double's range (sscanf) and "1" + 1 (daemon-failure).
flags="$flags -Wno-literal-range -Wno-string-plus-int"

# Its helpers (libtest.a): everything in src/common but the runner of
# its makefile, runtest.c, whose work run.c does.
objects=
for name in fdfill memfill mtest path print rand setrlim utf8 vmfill; do
	# shellcheck disable=SC2086 # one word per flag
	$CC $CFLAGS $flags -c "$PORT_SRC/src/common/$name.c" -o "$PORT_BUILD/common-$name.o"
	objects="$objects $PORT_BUILD/common-$name.o"
done

out="$SYSROOT/system/tests/libc-test"
rm -rf "$out"
mkdir -p "$out"
table="$PORT_BUILD/tests.h"
{
	echo "// Written by ports/libc-test/build.sh: the tests in run."
	echo "struct test {"
	echo "	const char *name;"
	echo "	int (*main)(int, char **);"
	echo "};"
} >"$table"
names=
for source in "$PORT_SRC"/src/functional/*.c "$PORT_SRC"/src/regression/*.c; do
	test=$(basename "$(dirname "$source")")/$(basename "$source" .c)
	case "$test" in
	*_dso) continue ;;
	functional/dlopen | functional/tls_init_dlopen | functional/tls_align_dlopen) continue ;;
	regression/tls_get_new-dtv) continue ;;
	functional/crypt | functional/iconv_open | regression/iconv-roundtrips) continue ;;
	functional/ipc_msg | functional/ipc_sem | functional/ipc_shm) continue ;;
	functional/socket | regression/dn_expand-empty | regression/dn_expand-ptr-0) continue ;;
	regression/syscall-sign-extend) continue ;;
	functional/pthread_cancel | functional/pthread_cancel-points) continue ;;
	regression/pthread_cancel-sem_wait | regression/pthread_cond_wait-cancel_ignored) continue ;;
	functional/pthread_robust | regression/pthread-robust-detach) continue ;;
	functional/pthread_mutex_pi) continue ;;
	functional/sem_open | regression/sem_close-unmap) continue ;;
	regression/flockfile-list | regression/malloc-brk-fail) continue ;;
	regression/pthread_atfork-errno-clobber) continue ;;
	esac
	name=$(echo "$test" | tr / -)
	symbol=libc_test_$(echo "$name" | tr - _)
	# shellcheck disable=SC2086 # one word per flag
	$CC $CFLAGS $flags "-Dmain=$symbol" -c "$source" -o "$PORT_BUILD/$name.o"
	# Everything but the renamed main is made local, so that the tests'
	# own globals do not clash.
	$OBJCOPY "--keep-global-symbol=$symbol" "$PORT_BUILD/$name.o"
	echo "int $symbol(int, char **);" >>"$table"
	names="$names $name:$symbol"
	objects="$objects $PORT_BUILD/$name.o"
	# tls_align's thread-local variables are in a library of their own.
	if [ "$test" = functional/tls_align ]; then
		# shellcheck disable=SC2086 # one word per flag
		$CC $CFLAGS $flags -c "$PORT_SRC/src/functional/tls_align_dso.c" \
			-o "$PORT_BUILD/functional-tls_align_dso.o"
		objects="$objects $PORT_BUILD/functional-tls_align_dso.o"
	fi
done
{
	echo "static const struct test TESTS[] = {"
	for entry in $names; do
		echo "	{\"${entry%%:*}\", ${entry#*:}},"
	done
	echo "};"
	echo "#define TEST_COUNT (sizeof(TESTS) / sizeof(TESTS[0]))"
} >>"$table"
# shellcheck disable=SC2086 # one word per object
$CC $CFLAGS -Wall -I"$PORT_BUILD" '-DSUITE="libc-test"' -o "$PORT_BUILD/run" \
	"$PORT_DIR/../mlibc-tests/run.c" $objects -lm -lpthread
# The image's copy without symbols; the build's keeps them, for a debugger.
$STRIP -o "$out/run" "$PORT_BUILD/run"
echo "libc-test: $(echo "$names" | wc -w) tests in /system/tests/libc-test"
