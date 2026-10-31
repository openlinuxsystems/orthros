// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// Asking authd(8) over its socket, as the boot tests of accounts and
// sign-in do (ADR 0218, 0250): a request is its fields, each ending in a
// NUL, and the answer one line.

#ifndef POSIX_AUTHD_H
#define POSIX_AUTHD_H

#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>

#include "check.h"

#define SOCKET "/system/live/authd/socket"

// Connects to authd and sends it the request of `len` bytes; the
// connection.
static int send_request(const char *request, size_t len) {
	int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
	struct sockaddr_un address = {.sun_family = AF_UNIX};
	strcpy(address.sun_path, SOCKET);
	CHECK(fd >= 0 && connect(fd, (struct sockaddr *)&address, sizeof(address)) == 0);
	CHECK(write(fd, request, len) == (ssize_t)len);
	CHECK(shutdown(fd, SHUT_WR) == 0);
	return fd;
}

// The answer on the connection `fd`, which it closes.
static void read_answer(int fd, char *answer, size_t size) {
	size_t len = 0;
	ssize_t got;
	while(len < size - 1 && (got = read(fd, answer + len, size - 1 - len)) > 0)
		len += got;
	answer[len] = 0;
	close(fd);
}

// A request's text and length: its fields, each ending in NUL.
#define REQUEST(text) text, sizeof(text) - 1

// Waits for authd's socket.
static void authd_answers(void) {
	struct stat st;
	long long deadline = check_now_ns() + 60 * 1000000000LL;
	while(stat(SOCKET, &st) != 0 && check_now_ns() < deadline)
		check_sleep_ms(100);
	CHECK(S_ISSOCK(st.st_mode) && (st.st_mode & 0777) == 0666 && st.st_uid == 0);
}

#endif
