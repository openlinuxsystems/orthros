#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Makes the test certificates and signatures of tls-test with OpenSSL, so
# that a TLS implementation other than the one under test made them: run
# it in this directory, and commit what it writes. Every key here is for
# the tests alone and protects nothing.
#
#   ca          the test authority, an RSA 2048 key, which tls-test trusts
#   intermediate
#               an authority with an ECDSA P-384 key, from ca
#   other-ca    an authority tls-test does not trust
#   ecdsa-p256, ed25519, rsa-2048
#               servers for tls.test and 127.0.0.1, from ca
#   ecdsa-p384  a server for them from intermediate
#   expired     a server from ca whose certificate ended in 2021
#   untrusted   a server from other-ca
#   client      a client certificate from ca
#   *.sig       OpenSSL's signatures of message.txt with each server's
#               key: ECDSA and RSA (PKCS #1 and PSS) of its SHA-256 or
#               SHA-384, and Ed25519's of the message itself
#
# Each certificate lasts 100 years from the day this ran.
set -eu

days=36500
authority() {
	openssl req -x509 -newkey rsa:2048 -nodes -keyout "$1.key" -out "$1.pem" \
		-days "$days" -subj "/O=Dukovo tests/CN=$2" -sha256 \
		-addext "basicConstraints=critical,CA:TRUE" \
		-addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null
}
key() {
	case "$2" in
	ecdsa-p256) openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$1.key" 2>/dev/null ;;
	ecdsa-p384) openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-384 -out "$1.key" 2>/dev/null ;;
	ed25519) openssl genpkey -algorithm ED25519 -out "$1.key" 2>/dev/null ;;
	rsa-2048) openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$1.key" 2>/dev/null ;;
	esac
}
extensions() {
	printf 'basicConstraints=critical,CA:FALSE\n'
	printf 'keyUsage=critical,digitalSignature\n'
	printf 'extendedKeyUsage=%s\n' "$1"
	if [ "$1" = serverAuth ]; then
		printf 'subjectAltName=DNS:tls.test,IP:127.0.0.1\n'
	fi
}
# A certificate for $1's key, from authority $2, for $3 (serverAuth or
# clientAuth), from $4 to $5 (openssl ca's YYYYMMDDHHMMSSZ) or for $days.
issue() {
	openssl req -new -key "$1.key" -subj "/O=Dukovo tests/CN=$1" -out "$1.csr"
	extensions "$3" >"$1.ext"
	if [ $# -gt 3 ]; then
		# Only openssl ca takes the dates a certificate starts and ends.
		mkdir -p ca.work
		: >ca.work/index
		printf '[ca]\ndefault_ca=test\n[test]\ndir=ca.work\ndatabase=ca.work/index\nnew_certs_dir=ca.work\nserial=ca.work/serial\ndefault_md=sha256\npolicy=any\nunique_subject=no\ncopy_extensions=none\n[any]\nO=optional\nCN=supplied\n' >ca.work/conf
		openssl rand -hex 16 >ca.work/serial
		openssl ca -batch -notext -config ca.work/conf -cert "$2.pem" -keyfile "$2.key" \
			-startdate "$4" -enddate "$5" -extfile "$1.ext" -in "$1.csr" -out "$1.pem" 2>/dev/null
		rm -r ca.work
	else
		openssl x509 -req -in "$1.csr" -CA "$2.pem" -CAkey "$2.key" -set_serial "0x$(openssl rand -hex 16)" \
			-days "$days" -sha256 -extfile "$1.ext" -out "$1.pem" 2>/dev/null
	fi
	rm "$1.csr" "$1.ext"
}

authority ca "Dukovo test CA"
authority other-ca "An authority tls-test does not trust"
key intermediate ecdsa-p384
openssl req -new -key intermediate.key -subj "/O=Dukovo tests/CN=Dukovo test intermediate" -out intermediate.csr
printf 'basicConstraints=critical,CA:TRUE,pathlen:0\nkeyUsage=critical,keyCertSign,cRLSign\n' >intermediate.ext
openssl x509 -req -in intermediate.csr -CA ca.pem -CAkey ca.key -set_serial "0x$(openssl rand -hex 16)" \
	-days "$days" -sha256 -extfile intermediate.ext -out intermediate.pem 2>/dev/null
rm intermediate.csr intermediate.ext
for kind in ecdsa-p256 ed25519 rsa-2048; do
	key "$kind" "$kind"
	issue "$kind" ca serverAuth
done
key ecdsa-p384 ecdsa-p384
issue ecdsa-p384 intermediate serverAuth
key expired ecdsa-p256
issue expired ca serverAuth 20200101000000Z 20210101000000Z
key untrusted ecdsa-p256
issue untrusted other-ca serverAuth
key client ecdsa-p256
issue client ca clientAuth
printf 'Signed by OpenSSL for tls-test, to be checked by ring on Radix.\n' >message.txt
openssl dgst -sha256 -sign ecdsa-p256.key -out ecdsa-p256.sig message.txt
openssl dgst -sha384 -sign ecdsa-p384.key -out ecdsa-p384.sig message.txt
openssl dgst -sha256 -sign rsa-2048.key -out rsa-2048-pkcs1.sig message.txt
openssl dgst -sha256 -sign rsa-2048.key -sigopt rsa_padding_mode:pss -sigopt rsa_pss_saltlen:digest \
	-out rsa-2048-pss.sig message.txt
openssl pkeyutl -sign -inkey ed25519.key -rawin -in message.txt -out ed25519.sig
# Only the authorities' certificates are kept, not their keys.
rm ca.key other-ca.key intermediate.key
