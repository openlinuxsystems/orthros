#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Installs the USB ID database where lsusb(8) reads it. `ob` runs it
# (ob(1), PORTS) with PORT_SRC and SYSROOT set.
set -eu

mkdir -p "$SYSROOT/system/resources/hwdata"
cp "$PORT_SRC/usb.ids" "$SYSROOT/system/resources/hwdata/usb.ids"
echo "usbids: $(sed -n 's/^#[[:space:]]*Version: //p' "$PORT_SRC/usb.ids") in /system/resources/hwdata/usb.ids"
