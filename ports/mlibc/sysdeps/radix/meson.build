# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# mlibc's system layer for Radix (ADR 0068), built as mlibc's
# sysdeps/radix: ports/mlibc/build.sh puts it there.

# Not mlibc's Linux interfaces: they need Linux's own headers, which are
# GPL-2.0 (ADR 0068), and Linux's file descriptor kinds (epoll, timerfd,
# signalfd, eventfd), which the kernel does not have.
sysdep_supported_options = {
	'posix': true,
	'linux': false,
	'glibc': true,
	'bsd': true,
}

radix_sources = files(
	'generic/core.cpp',
	'generic/files.cpp',
	'generic/radix.cpp',
)

# The dynamic linker's own copy, built with MLIBC_BUILDING_RTLD; a static
# program's C library has the dynamic linker's code too, and uses its own
# copy of these, as Linux's sysdeps do.
rtld_dso_sources += radix_sources

libc_sources += radix_sources + files(
	# The Linux option's getrandom (patch 0023) and memalign (0035),
	# which need nothing of Linux's.
	'../../options/linux/generic/malloc.cpp',
	'../../options/linux/generic/sys-random.cpp',
	'generic/entry.cpp',
	'generic/poll.cpp',
	'generic/process.cpp',
	'generic/socket.cpp',
	'generic/terminal.cpp',
	'x86_64/signal.S',
	'x86_64/thread-entry.S',
	'x86_64/thread.cpp',
)

if not no_headers
	# Linux's ABI: Radix takes Linux's numbers and structures
	# (radix-posix(7)). build.sh links each but ioctls.h and auxv.h,
	# Radix's own, to mlibc's abis/linux.
	install_headers(
		'include/abi-bits/access.h',
		'include/abi-bits/aio.h',
		'include/abi-bits/auxv.h',
		'include/abi-bits/blkcnt_t.h',
		'include/abi-bits/blksize_t.h',
		'include/abi-bits/clockid_t.h',
		'include/abi-bits/clone-flags.h',
		'include/abi-bits/dev_t.h',
		'include/abi-bits/epoll.h',
		'include/abi-bits/errno.h',
		'include/abi-bits/fcntl.h',
		'include/abi-bits/fd_set.h',
		'include/abi-bits/fsblkcnt_t.h',
		'include/abi-bits/fsfilcnt_t.h',
		'include/abi-bits/gid_t.h',
		'include/abi-bits/in.h',
		'include/abi-bits/ino_t.h',
		'include/abi-bits/inotify.h',
		'include/abi-bits/ioctls.h',
		'include/abi-bits/ipc.h',
		'include/abi-bits/limits.h',
		'include/abi-bits/mode_t.h',
		'include/abi-bits/mqueue.h',
		'include/abi-bits/msg.h',
		'include/abi-bits/nlink_t.h',
		'include/abi-bits/packet.h',
		'include/abi-bits/pid_t.h',
		'include/abi-bits/poll.h',
		'include/abi-bits/ptrace.h',
		'include/abi-bits/random.h',
		'include/abi-bits/reboot.h',
		'include/abi-bits/resource.h',
		'include/abi-bits/rlim_t.h',
		'include/abi-bits/route.h',
		'include/abi-bits/sa_family_t.h',
		'include/abi-bits/sched_param.h',
		'include/abi-bits/seek-whence.h',
		'include/abi-bits/sem.h',
		'include/abi-bits/shm.h',
		'include/abi-bits/sig-limits.h',
		'include/abi-bits/sigevent.h',
		'include/abi-bits/signal.h',
		'include/abi-bits/sigset_t.h',
		'include/abi-bits/sigval.h',
		'include/abi-bits/sockaddr_storage.h',
		'include/abi-bits/socket.h',
		'include/abi-bits/socklen_t.h',
		'include/abi-bits/stat.h',
		'include/abi-bits/statfs.h',
		'include/abi-bits/statvfs.h',
		'include/abi-bits/statx.h',
		'include/abi-bits/suseconds_t.h',
		'include/abi-bits/termios.h',
		'include/abi-bits/time.h',
		'include/abi-bits/uid_t.h',
		'include/abi-bits/utmp-defines.h',
		'include/abi-bits/utmpx.h',
		'include/abi-bits/utsname.h',
		'include/abi-bits/vm-flags.h',
		'include/abi-bits/vt.h',
		'include/abi-bits/wait.h',
		'include/abi-bits/xattr.h',
		subdir: 'abi-bits',
		follow_symlinks: true,
	)
	# Headers of the Linux option that other systems have too: build.sh
	# links them to it.
	install_headers(
		'include/sys/random.h',
		'include/sys/sysmacros.h',
		subdir: 'sys',
		follow_symlinks: true,
	)
	# And its malloc.h, of memalign and the glibc option's
	# malloc_usable_size.
	install_headers(
		'include/malloc.h',
		follow_symlinks: true,
	)
	install_headers(
		'include/asm/ioctl.h',
		'include/asm/ioctls.h',
		subdir: 'asm',
	)
endif

if not headers_only
	foreach crt : [['crt1.S', 'crt1.o'], ['crt1.S', 'Scrt1.o'], ['crti.S', 'crti.o'],
			['crtn.S', 'crtn.o']]
		custom_target(crt[1],
			build_by_default: true,
			command: c_compiler.cmd_array() + ['-c', '-o', '@OUTPUT@', '@INPUT@'],
			input: 'x86_64' / crt[0],
			output: crt[1],
			install: true,
			install_dir: get_option('libdir'),
		)
	endforeach
endif
