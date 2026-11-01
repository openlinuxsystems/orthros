// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// What the ssh boot test runs (ADR 0289): OpenSSH's client against the
// host's sshd, which QEMU's network runs for each connection to
// 10.0.2.100 port 22 (`sshd -i`). It makes an Ed25519 key with
// ssh-keygen and shows the public half, which the host puts in the
// server's authorized keys; signs and checks a file with the key, and a
// git commit; keeps the key in ssh-agent; signs in, takes the server's
// host key and finds it again, and refuses a host whose key changed;
// runs commands there, copies files both ways with scp and sftp, reads
// the host key with ssh-keyscan, and clones a repository of the host's
// over ssh, commits to it and pushes back, which the host checks. The
// host's user name, which the server signs in as, comes from port 79.

#include <arpa/inet.h>
#include <netinet/in.h>
#include <pwd.h>
#include <sys/socket.h>

#include "programs.h"

#define HOST "10.0.2.100"

// The user's folder, as the accounts give it: ssh finds its files there,
// whatever HOME says.
static char home[256];

// `name` in the user's folder.
static const char *at_home(const char *name) {
	static char path[2][320];
	static int turn;
	turn ^= 1;
	snprintf(path[turn], sizeof(path[turn]), "%s/%s", home, name);
	return path[turn];
}

// The name of the host's user, from the host's port 79.
static void host_user(char *name, size_t size) {
	int fd = socket(AF_INET, SOCK_STREAM, 0);
	CHECK(fd >= 0);
	struct sockaddr_in at = {.sin_family = AF_INET, .sin_port = htons(79)};
	CHECK(inet_pton(AF_INET, HOST, &at.sin_addr) == 1);
	CHECK(connect(fd, (struct sockaddr *)&at, sizeof(at)) == 0);
	size_t len = 0;
	ssize_t got;
	while(len < size - 1 && (got = read(fd, name + len, size - 1 - len)) > 0)
		len += got;
	close(fd);
	name[len] = 0;
	name[strcspn(name, "\r\n")] = 0;
	CHECK(name[0] != 0);
}

// Prints `what`, then the first line of the file at `path`.
static void show(const char *what, const char *path) {
	char line[512] = "";
	FILE *file = fopen(path, "r");
	CHECK(file != NULL);
	if(file) {
		if(!fgets(line, sizeof(line), file))
			line[0] = 0;
		fclose(file);
	}
	line[strcspn(line, "\n")] = 0;
	printf("%s%s\n", what, line);
	fflush(stdout);
}

// The client's version, and a key of its own.
static void keys(void) {
	CONTAINS("OpenSSH_10.6p1", "/bin/sh", "-c", "ssh -V 2>&1");
	STATUS(0, "/bin/sh", "-c",
			"mkdir -p ~/.ssh && ssh-keygen -q -t ed25519 -N '' -C sshtest@dukovo -f ~/.ssh/id_ed25519");
	CONTAINS("256 SHA256:", "/bin/sh", "-c", "ssh-keygen -l -f ~/.ssh/id_ed25519.pub");
	CONTAINS("sshtest@dukovo (ED25519)", "/bin/sh", "-c", "ssh-keygen -l -f ~/.ssh/id_ed25519");
	// The public half, for the host to let in.
	show("sshtest: key ", at_home(".ssh/id_ed25519.pub"));
	// A key with a passphrase opens only with it.
	STATUS(0, "/bin/sh", "-c", "ssh-keygen -q -t ed25519 -N 'open sesame' -f ~/locked");
	STATUS(255, "/bin/sh", "-c", "ssh-keygen -y -P wrong -f ~/locked 2>/dev/null");
	CONTAINS("ssh-ed25519 AAAA", "/bin/sh", "-c", "ssh-keygen -y -P 'open sesame' -f ~/locked");
}

// Signatures: a file's with ssh-keygen -Y, and a git commit's.
static void signatures(void) {
	OUTPUT("", "Good \"file\" signature for test@dukovo with ED25519 key SHA256:\n",
			"/bin/sh", "-c",
			"cd ~ && echo signed >note && ssh-keygen -q -Y sign -n file -f .ssh/id_ed25519 note && "
			"printf 'test@dukovo %s\\n' \"$(cat .ssh/id_ed25519.pub)\" >signers && "
			"ssh-keygen -Y verify -n file -f signers -I test@dukovo -s note.sig <note | cut -c1-62");
	STATUS(255, "/bin/sh", "-c",
			"cd ~ && echo changed >note && ssh-keygen -Y verify -n file -f signers -I test@dukovo "
			"-s note.sig <note >/dev/null 2>&1");
	OUTPUT("", "G\n", "/bin/sh", "-c",
			"cd ~ && git init -q signed && cd signed && git config gpg.format ssh && "
			"git config user.signingkey ~/.ssh/id_ed25519.pub && "
			"git config gpg.ssh.allowedSignersFile ~/signers && git config user.email test@dukovo && "
			"git config user.name Test && echo a >a && git add a && git commit -q -S -m Signed && "
			"git log -1 --format=%G?");
}

// ssh-agent keeps the key, and ssh-add lists and forgets it. The agent
// runs for a shell of its own and ends with it.
static void agent(void) {
	OUTPUT("", "256 (ED25519)\n1\n", "/bin/sh", "-c",
			"ssh-agent /bin/sh -c 'ssh-add -q ~/.ssh/id_ed25519 && ssh-add -l | cut -d\" \" -f1,4; "
			"ssh-add -q -D 2>/dev/null; ssh-add -l >/dev/null; echo $?'");
}

// Signing in to the host, once it has the key: its host key is taken
// the first time and found the next, and a changed one is refused.
static void sign_in(void) {
	int tries = 0;
	while(run_program((const char *const[]){"/bin/sh", "-c",
			"ssh -o BatchMode=yes test true 2>/dev/null", NULL}, "", 0) != 0) {
		CHECK(++tries < 60);
		if(tries >= 60)
			return;
		sleep(1);
	}
	OUTPUT("", "hello from the test's sshd\n", "/bin/sh", "-c", "ssh -T test </dev/null");
	OUTPUT("", "on the host\n", "/bin/sh", "-c", "ssh test echo on the host");
	// Options after the host are the command's, not ssh's.
	OUTPUT("", ".\n", "/bin/sh", "-c", "ssh test ls -d .");
	CONTAINS(HOST " ssh-ed25519 AAAA", "/bin/sh", "-c", "cat ~/.ssh/known_hosts");
	OUTPUT("", "again\n", "/bin/sh", "-c",
			"ssh -o StrictHostKeyChecking=yes test echo again");
	// The host's key, as ssh-keyscan reads it, is the one ssh took.
	OUTPUT("", "same\n", "/bin/sh", "-c",
			"ssh-keyscan -q -t ed25519 " HOST " 2>/dev/null >scanned; "
			"[ \"$(cut -d' ' -f3 scanned)\" = \"$(cut -d' ' -f3 ~/.ssh/known_hosts)\" ] && echo same");
	// A host whose key changed is not signed in to.
	CONTAINS("REMOTE HOST IDENTIFICATION HAS CHANGED", "/bin/sh", "-c",
			"ssh-keygen -q -t ed25519 -N '' -f ~/other && "
			"printf '" HOST " %s\\n' \"$(cut -d' ' -f1,2 ~/other.pub)\" >~/changed && "
			"ssh -o UserKnownHostsFile=~/changed -o StrictHostKeyChecking=yes test true 2>&1; "
			"echo status $?");
}

// Files both ways: scp and sftp.
static void copies(void) {
	OUTPUT("", "from the host\n", "/bin/sh", "-c",
			"cd ~ && scp -q test:greeting greeting && cat greeting");
	OUTPUT("", "", "/bin/sh", "-c",
			"cd ~ && echo from dukovo >sent && scp -q sent test:sent");
	OUTPUT("", "from dukovo\n", "/bin/sh", "-c", "ssh test cat sent");
	OUTPUT("", "from the host\n", "/bin/sh", "-c",
			"cd ~ && printf 'get greeting fetched\\n' | sftp -q -b - test >/dev/null && cat fetched");
}

// A repository over ssh: cloned, committed to and pushed back.
static void repository(void) {
	OUTPUT("", "First\n", "/bin/sh", "-c",
			"cd ~ && git clone -q test:repo.git clone && git -C clone log --format=%s");
	OUTPUT("", "", "/bin/sh", "-c",
			"cd ~/clone && git config user.email test@dukovo && git config user.name Test && "
			"echo from dukovo >>file && git commit -q -am 'From Dukovo' && git push -q origin main");
	OUTPUT("", "", "/bin/sh", "-c", "git -C ~/clone rev-parse HEAD >~/pushed");
	show("sshtest: pushed ", at_home("pushed"));
	OUTPUT("", "From Dukovo\nFirst\n", "/bin/sh", "-c",
			"cd ~ && git ls-remote test:repo.git refs/heads/main | cut -c1-40 >remote && "
			"[ \"$(cat remote)\" = \"$(git -C clone rev-parse HEAD)\" ] && git -C clone log --format=%s");
}

int main(void) {
	check_start("sshtest");
	struct passwd *me = getpwuid(getuid());
	CHECK(me != NULL);
	snprintf(home, sizeof(home), "%s", me ? me->pw_dir : "/tmp");
	setenv("HOME", home, 1);
	STATUS(0, "/bin/mkdir", "-p", at_home(".ssh"));
	char user[64];
	host_user(user, sizeof(user));
	// The host, by a name of its own.
	FILE *config = fopen(at_home(".ssh/config"), "w");
	CHECK(config != NULL);
	if(!config)
		return check_end();
	fprintf(config,
			"Host test\n\tHostName " HOST "\n\tUser %s\n\tStrictHostKeyChecking accept-new\n"
			"\tLogLevel ERROR\n",
			user);
	fclose(config);
	RUN(keys);
	RUN(signatures);
	RUN(agent);
	RUN(sign_in);
	RUN(copies);
	RUN(repository);
	return check_end();
}
