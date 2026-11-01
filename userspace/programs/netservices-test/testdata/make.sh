#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Makes the certificates of netservices-test's servers with OpenSSL: run
# it in this directory, and commit what it writes. The keys are for the
# tests alone and protect nothing.
#
#   ca          the test authority, an ECDSA P-256 key, which the test
#               has resolved(8), timed(8) and curl(1) trust; its key is
#               thrown away
#   server      the servers' certificate, from ca, for dns.test,
#               time.test, www.test and 127.0.0.1
#
# Each certificate lasts 100 years from the day this ran.
set -eu

days=36500
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
	-keyout ca.key -out ca.pem -days "$days" -subj "/O=Dukovo tests/CN=Network services test authority" \
	-sha256 -addext "basicConstraints=critical,CA:TRUE" \
	-addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out server.key 2>/dev/null
openssl req -new -key server.key -subj "/O=Dukovo tests/CN=dns.test" -out server.csr
printf '%s\n' 'basicConstraints=critical,CA:FALSE' 'keyUsage=critical,digitalSignature' \
	'extendedKeyUsage=serverAuth' 'subjectAltName=DNS:dns.test,DNS:time.test,DNS:www.test,IP:127.0.0.1' \
	>server.ext
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days "$days" \
	-sha256 -extfile server.ext -out server.pem 2>/dev/null
# PKCS #8, as rustls reads every kind of key.
openssl pkcs8 -topk8 -nocrypt -in server.key -out server.pk8
mv server.pk8 server.key
rm -f ca.key ca.srl server.csr server.ext
