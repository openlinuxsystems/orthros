#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Writes the root certificates of webpki-root-certs where programs read
# them, as one PEM file (dukovo-certificates(7)). `ob` runs it (ob(1),
# PORTS) with PORT_DIR, PORT_SRC and SYSROOT set.
set -eu

out="$SYSROOT/system/resources/certificates"
mkdir -p "$out"
python3 -I "$PORT_DIR/pem.py" "$PORT_SRC/webpki-root-certs/src/lib.rs" "$out/ca-certificates.crt.new"
mv "$out/ca-certificates.crt.new" "$out/ca-certificates.crt"
echo "ca-certificates: $(grep -c -- '-----BEGIN CERTIFICATE-----' "$out/ca-certificates.crt") root certificates in /system/resources/certificates/ca-certificates.crt"
