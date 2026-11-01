# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

# Builds obsd_fns.c for model.c: OpenBSD's rge(4) setup functions, as
# they are in sys/dev/pci/if_rge.c, with the #defines of if_rgereg.h,
# for the check that radix-rge-net's port makes the same register
# accesses (kernel/drivers/rge/src/tests.rs, openbsd_sequence). See
# model.c for how to run it. With "hash", prints the FNV-1a hash and the
# length of each log model wrote instead, as that test's OPENBSD table
# holds them.
#
#   python3 -I make_harness.py <OpenBSD's sys/dev/pci> <out>/obsd_fns.c
#   python3 -I make_harness.py hash <out>
import re, sys

if sys.argv[1] == 'hash':
    for name in ['r25', 'r25b', 'r25d1', 'r25d2', 'r26_1', 'r26_2']:
        text = open(f'{sys.argv[2]}/c_{name}.log', 'rb').read()
        h = 0xcbf29ce484222325
        for byte in text:
            h = ((h ^ byte) * 0x100000001b3) & 0xffffffffffffffff
        print(f'{name}: {h:#018x}, {text.count(10)}')
    sys.exit()

src_dir, out = sys.argv[1], sys.argv[2]
hdr = open(src_dir + '/if_rgereg.h').read()
c = open(src_dir + '/if_rge.c').read()

# Every #define of the header, joined across continuations, except the
# register access ones the harness defines itself.
defs = []
lines = hdr.split('\n')
i = 0
while i < len(lines):
    l = lines[i]
    if l.startswith('#define'):
        full = l
        while full.endswith('\\'):
            i += 1
            full = full[:-1] + ' ' + lines[i].strip()
        if 'bus_space' not in full and not full.startswith('#define __IF'):
            defs.append(full)
    i += 1
m = re.search(r'enum rge_mac_type \{.*?\};', hdr, re.S)
enum = m.group(0)
m = re.search(r'static const struct \{\s*uint16_t\s*reg;\s*uint16_t\s*val;\s*\} mac_r25_ephy\[\].*?\n\};', hdr, re.S)
ephy = m.group(0)

def fn(name):
    ls = c.split('\n')
    for k, l in enumerate(ls):
        if l.startswith(name + '('):
            # Include the return type line above.
            body = [ls[k - 1]]
            for l2 in ls[k:]:
                body.append(l2)
                if l2 == '}':
                    break
            return '\n'.join(body)
    raise SystemExit('no ' + name)

funcs = ['rge_reset', 'rge_chipinit', 'rge_set_phy_power', 'rge_ephy_config',
         'rge_ephy_config_mac_r25', 'rge_ephy_config_mac_r25b', 'rge_phy_config',
         'rge_phy_config_mac_r26_1', 'rge_phy_config_mac_r26_2', 'rge_phy_config_mac_r25',
         'rge_phy_config_mac_r25b', 'rge_phy_config_mac_r25d_1', 'rge_phy_config_mac_r25d_2',
         'rge_set_macaddr', 'rge_hw_init', 'rge_hw_reset', 'rge_disable_phy_ocp_pwrsave',
         'rge_patch_phy_mcu', 'rge_disable_aspm_clkreq', 'rge_exit_oob',
         'rge_write_csi', 'rge_read_csi', 'rge_write_mac_ocp', 'rge_read_mac_ocp',
         'rge_write_ephy', 'rge_read_ephy', 'rge_write_phy', 'rge_read_phy',
         'rge_write_phy_ocp', 'rge_read_phy_ocp']
bodies = [fn(f) for f in funcs]
text = '\n\n'.join(bodies)
# The only edit to OpenBSD's code: rge_phy_config's read of the PHY's RAM
# code version, which only decides whether to load Realtek's patches.
old = '\t/* Read ram code version. */\n\trge_write_phy_ocp(sc, 0xa436, 0x801e);\n\tsc->rge_rcodever = rge_read_phy_ocp(sc, 0xa438);\n'
assert text.count(old) == 1
text = text.replace(old, '')

pre = r'''
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <errno.h>
typedef uint8_t u_int8_t; typedef uint16_t u_int16_t; typedef uint32_t u_int32_t;
#define nitems(a) (sizeof(a) / sizeof((a)[0]))
#define ETHER_ADDR_LEN 6
#define MII_BMCR 0x00
#define MII_ANAR 0x04
#define MII_100T2CR 0x09
#define BMCR_RESET 0x8000
#define BMCR_AUTOEN 0x1000
#define BMCR_PDOWN 0x0800
#define BMCR_STARTNEG 0x0200
#define ANAR_10 0x0020
#define ANAR_10_FD 0x0040
#define ANAR_TX 0x0080
#define ANAR_TX_FD 0x0100
#define ANAR_FC 0x0400
#define ANAR_PAUSE_ASYM 0x0800
#define GTCR_ADV_1000THDX 0x0100
#define GTCR_ADV_1000TFDX 0x0200
'''
pre += enum + '\n'
pre += r'''
struct rge_softc {
	int rge_btag, rge_bhandle;
	enum rge_mac_type rge_type;
	uint64_t rge_mcodever;
	uint16_t rge_rcodever;
	uint32_t rge_flags;
	struct { char dv_xname[8]; } sc_dev;
};
void bus_space_write_1(int t, int h, uint32_t reg, uint8_t v);
void bus_space_write_2(int t, int h, uint32_t reg, uint16_t v);
void bus_space_write_4(int t, int h, uint32_t reg, uint32_t v);
uint8_t bus_space_read_1(int t, int h, uint32_t reg);
uint16_t bus_space_read_2(int t, int h, uint32_t reg);
uint32_t bus_space_read_4(int t, int h, uint32_t reg);
void DELAY(int us);
#define RGE_WRITE_4(sc, reg, val) bus_space_write_4(sc->rge_btag, sc->rge_bhandle, reg, val)
#define RGE_WRITE_2(sc, reg, val) bus_space_write_2(sc->rge_btag, sc->rge_bhandle, reg, val)
#define RGE_WRITE_1(sc, reg, val) bus_space_write_1(sc->rge_btag, sc->rge_bhandle, reg, val)
#define RGE_READ_4(sc, reg) bus_space_read_4(sc->rge_btag, sc->rge_bhandle, reg)
#define RGE_READ_2(sc, reg) bus_space_read_2(sc->rge_btag, sc->rge_bhandle, reg)
#define RGE_READ_1(sc, reg) bus_space_read_1(sc->rge_btag, sc->rge_bhandle, reg)
/* Realtek's microcode, which the port leaves out. */
static void rge_mac_config_mcu(struct rge_softc *sc, enum rge_mac_type t) { (void)sc; (void)t; }
static void rge_mac_config_ext_mcu(struct rge_softc *sc, enum rge_mac_type t) { (void)sc; (void)t; }
static void rge_phy_config_mcu(struct rge_softc *sc, uint16_t v) { (void)sc; (void)v; }
/* The RTL8127's, which the port leaves out. */
static void rge_ephy_config_mac_r27(struct rge_softc *sc) { (void)sc; abort(); }
static void rge_phy_config_mac_r27(struct rge_softc *sc) { (void)sc; abort(); }
'''
pre += '\n'.join(defs) + '\n'
pre += ephy + '\n'
protos = []
for b in bodies:
    head = b.split('\n')[0:2]
    protos.append(head[0] + ' ' + head[1].rstrip() + ';' if '(' in head[1] and head[1].rstrip().endswith(')') else '')
pre += '\n'.join(p.replace('static ', '').replace('static\t', '') for p in protos if p) + '\n'
open(out, 'w').write(pre + '\n' + text.replace('static void\n', 'void\n').replace('static int\n', 'int\n').replace('static uint16_t\n', 'uint16_t\n') + '\n')
