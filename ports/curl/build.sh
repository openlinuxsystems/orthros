#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds curl and libcurl for Radix (ob(1), PORTS), static, with rustls
# (rustls-ffi), zlib and zstd, the threaded resolver, which asks the C
# library and so resolved(8), and the root certificates of
# /etc/ssl/certs/ca-certificates.crt (dukovo-certificates(7)). Without
# a library for each: HTTP/2 and HTTP/3 (nghttp2, ngtcp2), brotli,
# international domain names (libidn2), the public suffix list
# (libpsl), SSH (libssh2), LDAP and RTMP.
set -eu

cd "$PORT_BUILD"
x86_64-radix-configure "$PORT_SRC" --disable-shared --enable-static \
	--with-rustls --without-openssl --with-zlib --with-zstd \
	--without-brotli --without-nghttp2 --without-nghttp3 --without-ngtcp2 \
	--without-libidn2 --without-libpsl --without-libssh2 --without-libssh \
	--without-librtmp --disable-ldap --disable-ldaps \
	--enable-threaded-resolver --enable-ipv6 --enable-websockets \
	--with-ca-bundle=/etc/ssl/certs/ca-certificates.crt --without-ca-path \
	>configure.log 2>&1 || {
	tail -n 40 configure.log >&2
	exit 1
}
# TLS must be rustls's, compression zlib's and zstd's, and names looked
# up on a thread, so a slow lookup does not hold the transfer.
for feature in USE_RUSTLS HAVE_LIBZ HAVE_ZSTD HAVE_THREADS_POSIX USE_RESOLV_THREADED; do
	grep -q "^#define $feature 1" lib/curl_config.h || {
		echo "curl's configure did not set $feature" >&2
		exit 1
	}
done
make -j"$JOBS" >make.log 2>&1 || {
	tail -n 60 make.log >&2
	exit 1
}
make install DESTDIR="$SYSROOT" >/dev/null
rm -f "$SYSROOT/system/libraries/libcurl.la"
# libcurl's flags for programs built against it, by pkg-config(1) or
# curl-config, without the build machine's path to the sysroot, which
# the compiler searches already.
sed -i "s| -L$SYSROOT/system/libraries||g" \
	"$SYSROOT/system/libraries/pkgconfig/libcurl.pc" "$SYSROOT/system/commands/curl-config"
$STRIP "$SYSROOT/system/commands/curl"
