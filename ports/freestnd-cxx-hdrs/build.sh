#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Installs the x86-64 headers into the toolchain, as include/c++ (ob(1),
# PORTS, has what ob passes).
set -eu
rm -rf "$TOOLCHAIN/include/c++"
mkdir -p "$TOOLCHAIN/include"
cp -R "$PORT_SRC/x86_64/include" "$TOOLCHAIN/include/c++"
