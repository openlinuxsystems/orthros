# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

"""Writes webpki-root-certs' certificates as one PEM file.

The crate's src/lib.rs holds each root certificate as a byte string of
\\xNN escapes, after a comment with its name:

    // "ISRG Root X1"
    CertificateDer::from_slice(b"\\x30\\x82..."),

Usage: python3 -I pem.py SRC/webpki-root-certs/src/lib.rs OUT. Each
certificate is written as it is, DER in base64, after a line with its
name, in the crate's order.
"""

import base64
import re
import sys

ENTRY = re.compile(
    r'^[ \t]*//[ \t]*"(?P<name>[^\n]*)"[ \t]*\n'
    r'[ \t]*CertificateDer::from_slice\(b"(?P<der>(?:\\x[0-9A-Fa-f]{2})+)"\),',
    re.MULTILINE,
)

HEADER = """\
# The root certificates Mozilla trusts for TLS servers, as the Common CA
# Database (CCADB) publishes them, from the rustls project's
# webpki-root-certs crate, under the Community Data License Agreement -
# Permissive 2.0 (/system/resources/licenses). Made by Dukovo's
# ports/ca-certificates; dukovo-certificates(7) says how to trust others.
"""


def main():
    source, out = sys.argv[1], sys.argv[2]
    with open(source, encoding="utf-8") as file:
        text = file.read()
    entries = ENTRY.findall(text)
    # Every certificate in the file must have been read: one the pattern
    # missed would be left out without a word.
    if not entries or len(entries) != text.count("CertificateDer::from_slice("):
        sys.exit(f"pem.py: {source}: read {len(entries)} of "
                 f"{text.count('CertificateDer::from_slice(')} certificates")
    with open(out, "w", encoding="utf-8", newline="\n") as file:
        file.write(HEADER)
        for name, escaped in entries:
            der = bytes.fromhex(escaped.replace("\\x", ""))
            # A DER SEQUENCE with a long length, as every certificate is.
            if der[0] != 0x30 or der[1] not in (0x81, 0x82):
                sys.exit(f"pem.py: {name} is not a DER certificate")
            body = base64.b64encode(der).decode("ascii")
            lines = [body[i:i + 64] for i in range(0, len(body), 64)]
            file.write(f"\n# {name}\n-----BEGIN CERTIFICATE-----\n")
            file.write("\n".join(lines))
            file.write("\n-----END CERTIFICATE-----\n")


if __name__ == "__main__":
    main()
