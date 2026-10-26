#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds the Berkeley DB 1.85 library for Radix (ob(1), PORTS) and
# installs libdb1.a, and db.h in /system/include/db1 as Debian does, into
# the sysroot for nvi. Its only build was the GNU C library's, so this
# compiles what dbopen() needs: the btree, hash and recno access methods
# and the page cache; not the ndbm interface. The code is in the C of
# before prototypes, so it is compiled as C17, which still takes it. The
# port's own compat.h stands in for the one the GNU C library wrote.
set -eu

rm "$PORT_SRC/compat.h"
cd "$PORT_BUILD"
for source in btree/bt_close btree/bt_conv btree/bt_debug btree/bt_delete btree/bt_get \
	btree/bt_open btree/bt_overflow btree/bt_page btree/bt_put btree/bt_search \
	btree/bt_seq btree/bt_split btree/bt_utils db/db hash/hash hash/hash_bigkey \
	hash/hash_buf hash/hash_func hash/hash_log2 hash/hash_page mpool/mpool \
	recno/rec_close recno/rec_delete recno/rec_get recno/rec_open recno/rec_put \
	recno/rec_search recno/rec_seq recno/rec_utils; do
	$CC $CFLAGS -std=gnu17 -Wno-deprecated-non-prototype -D_GNU_SOURCE \
		-D__DBINTERFACE_PRIVATE -I"$PORT_DIR" -I"$PORT_SRC" \
		-c "$PORT_SRC/$source.c" -o "${source#*/}.o"
done
rm -f libdb1.a
$AR rcs libdb1.a ./*.o

mkdir -p "$SYSROOT/system/include/db1" "$SYSROOT/system/libraries"
cp "$PORT_SRC/db.h" "$SYSROOT/system/include/db1/"
cp libdb1.a "$SYSROOT/system/libraries/"
