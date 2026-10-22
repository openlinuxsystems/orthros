#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds the tests of mlibc's tests/meson.build that apply to Radix into
# one static program, /system/tests/libc/run (run.c), in which each test's
# main is renamed after it (posix-fork for tests/posix/fork.c). `ob` runs
# it (ob(1), PORTS) with CC, CFLAGS, OBJCOPY, PORT_DIR, PORT_SRC,
# PORT_BUILD and SYSROOT set.
#
# Left out, with why:
#   linux/*, linux-wrappers/*, glibc/linux-syscall: Linux's own
#     interfaces, which the C library leaves out on Radix (ADR 0068)
#   rtld/*, glibc/_dl_find_object: dynamic linking (bundles, later)
#   posix/popen, posix/system, posix/wordexp: they need /bin/sh (Phase 9)
#   posix/getaddrinfo, gethostbyname, getnameinfo, getservbyname,
#     getservbyport, protoent, accept4, cmsg, if_indextoname,
#     if_nameindex: networking (Phase 12)
#   posix/sysv_shm, sysv_msg, sysv_sem: System V IPC, which Radix does
#     not have
#   posix/pwd, posix/grp, glibc/shadow, glibc/getgrouplist,
#     posix/getlogin_r: the user database (Phase 13)
#   ansi/tz, glibc/error, glibc/error_at_line: mlibc runs them through a
#     Python wrapper that checks their output; ansi/tz needs time zones
#     (Phase 12)
#   ansi/fenv, posix/pthread_cancel: mlibc leaves them out of its cross
#     builds too
set -eu

out="$SYSROOT/system/tests/libc"
rm -rf "$out"
mkdir -p "$out"
table="$PORT_BUILD/tests.h"
{
	echo "// Written by ports/mlibc-tests/build.sh: the tests in run."
	echo "struct test {"
	echo "	const char *name;"
	echo "	int (*main)(int, char **);"
	echo "};"
} >"$table"
names=
objects=
for test in $(sed -n "/^all_test_cases = \[/,/^\]/s/^	'\([a-z-]*\/[A-Za-z0-9_-]*\)',.*/\1/p" \
	"$PORT_SRC/tests/meson.build"); do
	case "$test" in
	linux/* | linux-wrappers/* | rtld/* | glibc/_dl_find_object) continue ;;
	posix/popen | posix/system | posix/wordexp | posix/posix_spawn) continue ;;
	posix/getaddrinfo | posix/gethostbyname | posix/getnameinfo) continue ;;
	posix/getservbyname | posix/getservbyport | posix/protoent) continue ;;
	posix/accept4 | posix/cmsg | posix/if_indextoname | posix/if_nameindex) continue ;;
	posix/sysv_shm | posix/sysv_msg | posix/sysv_sem) continue ;;
	posix/pwd | posix/grp | glibc/shadow | glibc/getgrouplist) continue ;;
	posix/getlogin_r) continue ;;
	ansi/tz | glibc/error | glibc/error_at_line) continue ;;
	ansi/fenv | posix/pthread_cancel) continue ;;
	esac
	name=$(echo "$test" | tr / -)
	symbol=libc_test_$(echo "$test" | tr /- __)
	# As mlibc's tests/meson.build builds them, with the warnings it turns
	# off for two of them; then everything but the renamed main is made
	# local, so that the tests' own globals do not clash.
	extra=
	case "$test" in
	ansi/calloc) extra=-Wno-alloc-size-larger-than ;;
	ansi/strcmp) extra=-Wno-stringop-overflow ;;
	esac
	# shellcheck disable=SC2086 # $extra is one flag or none
	$CC $CFLAGS -D_GNU_SOURCE -fno-builtin -Wno-unknown-warning-option $extra \
		"-Dmain=$symbol" -c "$PORT_SRC/tests/$test.c" -o "$PORT_BUILD/$name.o"
	$OBJCOPY "--keep-global-symbol=$symbol" "$PORT_BUILD/$name.o"
	# Every test is called as main(argc, argv), whatever it declares: the
	# same call on x86-64 either way.
	echo "int $symbol(int, char **);" >>"$table"
	names="$names $name:$symbol"
	objects="$objects $PORT_BUILD/$name.o"
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
$CC $CFLAGS -Wall -I"$PORT_BUILD" -o "$PORT_BUILD/run" "$PORT_DIR/run.c" $objects -lm -lpthread
# The image's copy without symbols; the build's keeps them, for a debugger.
$STRIP -o "$out/run" "$PORT_BUILD/run"
echo "mlibc-tests: $(echo "$names" | wc -w) tests in /system/tests/libc/run"
