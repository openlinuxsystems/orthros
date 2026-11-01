#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds OpenSSH's client for Radix (ob(1), PORTS) and installs it into
# the folders Dukovo names (radix-files(7)): ssh, scp, sftp, ssh-add,
# ssh-agent, ssh-keygen and ssh-keyscan in /system/commands, their
# manual pages, and OpenSSH's ssh_config among the default settings
# Spark puts in /system/settings.
#
# It is the client only (ADR 0289): no sshd. It is built without OpenSSL,
# so it has OpenSSH's own cryptography alone (Ed25519, ChaCha20-Poly1305,
# AES, ML-KEM768 and sntrup761 with X25519), without PAM, Kerberos,
# security keys (libfido2) or PKCS#11 (which loads libraries).
set -eu

# The configure script is made from the source, which is the release's
# commit; the copy keeps the fetched source as it was. aclocal.m4 is in
# the source, so Autoconf alone makes it, without Automake.
rm -rf "$PORT_BUILD/src"
cp -R "$PORT_SRC" "$PORT_BUILD/src"
cd "$PORT_BUILD/src"
{ autoconf && autoheader; } >"$PORT_BUILD/autoconf.log" 2>&1 || {
	tail -n 40 "$PORT_BUILD/autoconf.log" >&2
	exit 1
}
# mlibc's getopt moves options from after the first argument to the
# front, as glibc's does: `ssh host ls -l` would take -l as ssh's. With
# optreset seen missing, OpenSSH uses its own getopt, which stops at
# the first argument, as on Linux.
x86_64-radix-configure . --without-openssl --with-zlib --without-pam \
	--without-selinux --without-kerberos5 --without-libedit --without-ldns \
	--disable-security-key --disable-pkcs11 --disable-strip \
	--sysconfdir=/system/settings/ssh --libexecdir=/system/libraries/ssh \
	--with-privsep-path=/system/state/empty --with-pid-dir=/system/live \
	--with-default-path=/system/commands \
	ac_cv_have_getopt_optreset=no \
	>"$PORT_BUILD/configure.log" 2>&1 || {
	tail -n 40 "$PORT_BUILD/configure.log" >&2
	tail -n 40 config.log >&2
	exit 1
}
programs="ssh scp sftp ssh-add ssh-agent ssh-keygen ssh-keyscan"
# shellcheck disable=SC2086 # one word per program
make -j"$JOBS" $programs >"$PORT_BUILD/make.log" 2>&1 || {
	tail -n 60 "$PORT_BUILD/make.log" >&2
	exit 1
}

stage="$PORT_BUILD/stage"
rm -rf "$stage"
mkdir -p "$stage/system/commands" "$stage/system/resources/man/man1" "$stage/system/resources/man/man5"
for program in $programs; do
	cp "$program" "$stage/system/commands/$program"
	$STRIP "$stage/system/commands/$program"
	cp "$program.1" "$stage/system/resources/man/man1/$program.1"
done
cp ssh_config.5 "$stage/system/resources/man/man5/ssh_config.5"

defaults="$stage/system/resources/default-settings/ssh"
mkdir -p "$defaults"
cp ssh_config "$defaults/ssh_config"

(cd "$stage" && find . ! -type d | sed 's|^\./||' | sort) >"$PORT_INFO/image"
cp -R "$stage/." "$SYSROOT/"
