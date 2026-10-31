dukovo-certificates(7) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

dukovo-certificates - the certificate authorities Dukovo trusts, and TLS

# DESCRIPTION

A program that connects with TLS, as *curl* does to an *https://*
address, checks the server's certificate against the root certificates
of the authorities it trusts. Dukovo trusts the authorities Mozilla
trusts for TLS servers, the list Firefox and most Linux systems use, as
the Common CA Database (CCADB) publishes it (ADR 0192). They are one
file:

_/system/resources/certificates/ca-certificates.crt_

Each certificate is in PEM, after a line with its name:

```
# ISRG Root X1
-----BEGIN CERTIFICATE-----
MIIFazCCA1OgAwIBAgIRAIIQz7DSQONZRGPgu2OCiwAwDQYJKoZIhvcNAQELBQAw
...
-----END CERTIFICATE-----
```

The file comes with the system and changes only with an update. Programs
from Linux look for it by the names Debian and Alpine give it, which the
default settings have as symbolic links to it:

_/etc/ssl/certs/ca-certificates.crt_
	Debian's name, which curl, Go's programs and OpenSSL's on Debian
	read.

_/etc/ssl/cert.pem_
	Alpine's and the BSDs' name.

_/etc_ is _/system/settings_, so these are
_/system/settings/ssl/certs/ca-certificates.crt_ and
_/system/settings/ssl/cert.pem_.

# TRUSTING OTHER AUTHORITIES

To trust an authority of your own, such as a company's, as well as
Mozilla's, make the settings' file a copy of the system's with your
authority's certificate added, in place of the link:

```
cd /system/settings/ssl/certs
cat /system/resources/certificates/ca-certificates.crt my-ca.pem >ca-certificates.crt.new
mv ca-certificates.crt.new ca-certificates.crt
ln -sf certs/ca-certificates.crt ../cert.pem
```

To trust fewer, leave out the ones you do not want. A file of your own
does not change with updates: to follow Mozilla's list again, make it a
link once more:

```
ln -sf /system/resources/certificates/ca-certificates.crt /system/settings/ssl/certs/ca-certificates.crt
```

# TLS

Dukovo's programs in Rust use rustls on ring for TLS, each linked into
the program, and programs in C use rustls through rustls-ffi. Both
speak TLS 1.2 and 1.3, with AES-GCM and ChaCha20-Poly1305, X25519, P-256
and P-384, and ECDSA, Ed25519 and RSA certificates. There is no OpenSSL.

# FILES

_/system/resources/certificates/ca-certificates.crt_
	The root certificates Dukovo trusts.

_/system/resources/licenses_
	Their license, the Community Data License Agreement - Permissive
	2.0, with the license manifest.

# SEE ALSO

*network*(5), *netd*(8), *spark*(8)
