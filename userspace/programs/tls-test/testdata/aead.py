# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

"""Writes aead.txt, the known answers of tls-test's AEAD checks.

Python's cryptography package, which OpenSSL's code runs under, seals
messages of the lengths below, the byte at i of each (7i + 3) mod 256,
with the key 00 01 02 ... and the nonce 64 65 ... 6f, and the extra data
"dukovo tls-test". aead.txt has a line for each: the algorithm, the
length and the SHA-256 of the sealed message and its tag. Lengths around
16 and 96 cross the block sizes of the ciphers' fast paths.

Usage: python3 aead.py >aead.txt
"""

import hashlib

from cryptography.hazmat.primitives.ciphers.aead import AESGCM, ChaCha20Poly1305

LENGTHS = [0, 1, 15, 16, 17, 95, 96, 97, 255, 1000, 4099, 65536]
KEY = bytes(range(32))
NONCE = bytes(range(100, 112))
AAD = b"dukovo tls-test"


def plain(length):
    return bytes((i * 7 + 3) & 0xFF for i in range(length))


for name, make, key_len in [
    ("AES-128-GCM", AESGCM, 16),
    ("AES-256-GCM", AESGCM, 32),
    ("CHACHA20-POLY1305", ChaCha20Poly1305, 32),
]:
    aead = make(KEY[:key_len])
    for length in LENGTHS:
        sealed = aead.encrypt(NONCE, plain(length), AAD)
        print(f"{name} {length} {hashlib.sha256(sealed).hexdigest()}")
