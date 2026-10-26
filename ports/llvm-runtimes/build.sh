#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds LLVM's C++ runtime for Radix (ob(1), PORTS; ADR 0090) with
# CMake, as LLVM's runtimes build does on its own, and installs it into
# the sysroot's /system/libraries: libc++, libc++abi and libunwind as
# static libraries, as every program in /system is static, with
# libc++abi on libunwind and all three on compiler-rt's builtins rather
# than GCC's. Threads are
# POSIX threads. The time zone database of std::chrono is left out until
# the system has tzdata (Phase 12). CMake is told that the C++ compiler
# works: it would check by linking a program, which x86_64-radix-c++
# links with libc++, not there until this has built it.
set -eu

# Nothing of an earlier build's headers in the way of this one's.
rm -rf "$SYSROOT/system/include/c++"
cd "$PORT_BUILD"
cmake -G Ninja -S "$PORT_SRC/runtimes" -B . -Wno-dev \
	-DCMAKE_TOOLCHAIN_FILE="$CMAKE_TOOLCHAIN_FILE" \
	-DCMAKE_BUILD_TYPE=Release \
	-DCMAKE_INSTALL_PREFIX="$PREFIX" \
	-DLIBUNWIND_INSTALL_LIBRARY_DIR=libraries \
	-DLIBCXXABI_INSTALL_LIBRARY_DIR=libraries \
	-DLIBCXX_INSTALL_LIBRARY_DIR=libraries \
	-DLIBCXX_INSTALL_MODULES_DIR=resources/libc++/v1 \
	-DCMAKE_CXX_COMPILER_WORKS=ON \
	-DLLVM_ENABLE_RUNTIMES="libunwind;libcxxabi;libcxx" \
	-DLLVM_INCLUDE_TESTS=OFF \
	-DLLVM_INCLUDE_DOCS=OFF \
	-DLIBUNWIND_ENABLE_SHARED=OFF \
	-DLIBUNWIND_USE_COMPILER_RT=ON \
	-DLIBUNWIND_INCLUDE_TESTS=OFF \
	-DLIBCXXABI_ENABLE_SHARED=OFF \
	-DLIBCXXABI_USE_LLVM_UNWINDER=ON \
	-DLIBCXXABI_USE_COMPILER_RT=ON \
	-DLIBCXXABI_INCLUDE_TESTS=OFF \
	-DLIBCXXABI_HAS_PTHREAD_API=ON \
	-DLIBCXX_ENABLE_SHARED=OFF \
	-DLIBCXX_CXX_ABI=libcxxabi \
	-DLIBCXX_USE_COMPILER_RT=ON \
	-DLIBCXX_INCLUDE_TESTS=OFF \
	-DLIBCXX_INCLUDE_BENCHMARKS=OFF \
	-DLIBCXX_HAS_PTHREAD_API=ON \
	-DLIBCXX_ENABLE_TIME_ZONE_DATABASE=OFF \
	>configure.log 2>&1 || {
	tail -n 40 configure.log >&2
	exit 1
}
ninja -j"$JOBS" >build.log 2>&1 || {
	tail -n 60 build.log >&2
	exit 1
}
DESTDIR="$SYSROOT" ninja install >install.log 2>&1
