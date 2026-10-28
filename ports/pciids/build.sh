#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Installs the PCI ID database where lspci(8) reads it. `ob` runs it
# (ob(1), PORTS) with PORT_SRC and SYSROOT set.
set -eu

mkdir -p "$SYSROOT/system/resources/hwdata"
cp "$PORT_SRC/pci.ids" "$SYSROOT/system/resources/hwdata/pci.ids"
echo "pciids: $(sed -n 's/^#[[:space:]]*Version: //p' "$PORT_SRC/pci.ids") in /system/resources/hwdata/pci.ids"
