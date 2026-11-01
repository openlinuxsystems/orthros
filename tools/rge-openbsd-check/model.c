/*
 * SPDX-FileCopyrightText: 2019, 2020, 2023-2026 Kevin Lo <kevlo@openbsd.org>
 * SPDX-FileCopyrightText: 2026 The Dukovo contributors
 * SPDX-License-Identifier: ISC
 *
 * Permission to use, copy, modify, and distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 */

/*
 * OpenBSD's rge(4) setup, its own functions, run against the register
 * model kernel/drivers/rge/src/tests.rs simulates, in the order
 * radix-rge-net's start() calls them: the log of every access and wait
 * it writes, chip by chip, is what that test's openbsd_sequence checks
 * the port's against, by hash. init_rest() is OpenBSD's rge_init() after
 * the PHY's configuration with the port's differences, which its
 * comment lists. Checked against if_rge.c revision 1.48 (OpenBSD's git
 * mirror, commit 21ea132db7eac1d478645cd6150c5787dc366ebc):
 *
 *   git clone --depth 1 --filter=blob:none --sparse https://github.com/openbsd/src
 *   git -C src sparse-checkout set sys/dev/pci
 *   python3 -I make_harness.py src/sys/dev/pci <out>/obsd_fns.c
 *   cc -I<out> -o <out>/model model.c && <out>/model <out>
 *   python3 -I make_harness.py hash <out>
 */

#include "obsd_fns.c"

static uint8_t regs[0x10000];
static uint16_t mac[0x8000], phy[0x8000], ephy[0x80];
static uint32_t csi[0x1000 / 4];
static uint32_t revision;
static FILE *out;

static void logop(const char *op, uint32_t at, uint32_t v) { fprintf(out, "%s %04x %x\n", op, at, v); }
void DELAY(int us) { fprintf(out, "d %d\n", us); }

static uint32_t get(uint32_t at, int n) { uint32_t v = 0; for (int i = 0; i < n; i++) v |= (uint32_t)regs[at + i] << (8 * i); return v; }
static void put(uint32_t at, int n, uint32_t v) { for (int i = 0; i < n; i++) regs[at + i] = v >> (8 * i); }

static void phy_store(uint16_t at, uint16_t v) {
	if (at == 0xa400) v &= ~0x8000;          /* BMCR's reset is done at once */
	if (at == 0xb820) phy[0xb800 >> 1] = (phy[0xb800 >> 1] & ~0x40) | ((v & 0x10) ? 0x40 : 0);
	phy[at >> 1] = v;
}

static void wr(uint32_t at, int n, uint32_t v) {
	if (at == RGE_ISR && n == 4) { put(at, 4, get(at, 4) & ~v); return; }
	put(at, n, v);
	if (at == RGE_CMD && n == 1) regs[at] &= ~RGE_CMD_RESET;
	if (at == RGE_MACOCP && n == 4) {
		uint16_t a = (v >> 16) & 0x7fff;
		if (v & RGE_MACOCP_BUSY) { mac[a] = v & 0xffff; put(at, 4, v & ~RGE_MACOCP_BUSY); }
		else put(at, 4, (v & 0xffff0000) | mac[a]);
	}
	if (at == RGE_PHYOCP && n == 4) {
		uint16_t a = (v >> 16) & 0x7fff;
		if (v & RGE_PHYOCP_BUSY) { phy_store(a << 1, v & 0xffff); put(at, 4, v & ~RGE_PHYOCP_BUSY); }
		else put(at, 4, RGE_PHYOCP_BUSY | (v & 0x7fff0000) | phy[a]);
	}
	if (at == RGE_EPHYAR && n == 4) {
		uint16_t a = (v >> 16) & 0x7f;
		if (v & RGE_EPHYAR_BUSY) { ephy[a] = v & 0xffff; put(at, 4, v & ~RGE_EPHYAR_BUSY); }
		else put(at, 4, RGE_EPHYAR_BUSY | (v & 0x7fff0000) | ephy[a]);
	}
	if (at == RGE_CSIAR && n == 4) {
		uint32_t a = (v & 0xfff) >> 2;
		if (v & RGE_CSIAR_BUSY) { csi[a] = get(RGE_CSIDR, 4); put(at, 4, v & ~RGE_CSIAR_BUSY); }
		else { put(RGE_CSIDR, 4, csi[a]); put(at, 4, v | RGE_CSIAR_BUSY); }
	}
}

static uint32_t rd(uint32_t at, int n) {
	uint32_t v = get(at, n);
	if (at == RGE_MCUCMD && n == 1) v |= 0x30;
	if (at == RGE_IM && n == 2) v |= 0x0103;
	if (at == RGE_TWICMD && n == 2) v |= 0x0200;
	if (at == RGE_TXCFG && n == 4) v = (v & ~RGE_TXCFG_HWREV) | revision;
	return v;
}

void bus_space_write_1(int t, int h, uint32_t at, uint8_t v) { (void)t; (void)h; logop("w1", at, v); wr(at, 1, v); }
void bus_space_write_2(int t, int h, uint32_t at, uint16_t v) { (void)t; (void)h; logop("w2", at, v); wr(at, 2, v); }
void bus_space_write_4(int t, int h, uint32_t at, uint32_t v) { (void)t; (void)h;
	/* The rings' addresses differ from run to run on the Rust side. */
	int ring = at == RGE_RXDESC_ADDR_LO || at == RGE_RXDESC_ADDR_HI || at == RGE_TXDESC_ADDR_LO || at == RGE_TXDESC_ADDR_HI;
	logop("w4", at, ring ? 0 : v); wr(at, 4, v); }
uint8_t bus_space_read_1(int t, int h, uint32_t at) { (void)t; (void)h; uint32_t v = rd(at, 1); logop("r1", at, v); return v; }
uint16_t bus_space_read_2(int t, int h, uint32_t at) { (void)t; (void)h; uint32_t v = rd(at, 2); logop("r2", at, v); return v; }
uint32_t bus_space_read_4(int t, int h, uint32_t at) { (void)t; (void)h; uint32_t v = rd(at, 4); logop("r4", at, v); return v; }

#define RX_RING 0x11112000ULL
#define TX_RING 0x22223000ULL
#define SLOT 2048

/* rge_init() after its rge_chipinit() and rge_phy_config(), for one
 * queue each way, with the port's differences: no VLAN tag stripping,
 * frames no longer than a slot, every multicast frame, IFM_AUTO, and
 * interrupts without moderation. */
static void init_rest(struct rge_softc *sc) {
	uint32_t rxconf, val, reg;
	int i, num_miti;

	RGE_SETBIT_1(sc, RGE_EECMD, RGE_EECMD_WRITECFG);

	RGE_CLRBIT_1(sc, 0xf1, 0x80);
	rge_disable_aspm_clkreq(sc);
	RGE_WRITE_2(sc, RGE_EEE_TXIDLE_TIMER,
	    9216 - 14 - 4 - 4 + 14 + 32);

	RGE_WRITE_4(sc, RGE_RXDESC_ADDR_LO, (uint32_t)RX_RING);
	RGE_WRITE_4(sc, RGE_RXDESC_ADDR_HI, (uint32_t)(RX_RING >> 32));
	RGE_WRITE_4(sc, RGE_TXDESC_ADDR_LO, (uint32_t)TX_RING);
	RGE_WRITE_4(sc, RGE_TXDESC_ADDR_HI, (uint32_t)(TX_RING >> 32));

	if (sc->rge_type == MAC_R25)
		rxconf = RGE_RXCFG_CONFIG;
	else if (sc->rge_type == MAC_R25B)
		rxconf = RGE_RXCFG_CONFIG_8125B;
	else if (RGE_TYPE_R25D(sc))
		rxconf = RGE_RXCFG_CONFIG_8125D;
	else
		rxconf = RGE_RXCFG_CONFIG_8126;
	RGE_WRITE_4(sc, RGE_RXCFG, rxconf);
	RGE_WRITE_4(sc, RGE_TXCFG, RGE_TXCFG_CONFIG);

	reg = rge_read_csi(sc, 0x70c) & ~0x3f000000;
	rge_write_csi(sc, 0x70c, reg | 0x27000000);

	if (RGE_TYPE_R26(sc) || RGE_TYPE_R27(sc)) {
		reg = rge_read_csi(sc, 0x890) & ~0x00000001;
		rge_write_csi(sc, 0x890, reg);
	} else if (!RGE_TYPE_R25D(sc))
		RGE_WRITE_2(sc, 0x0382, 0x221b);

	RGE_WRITE_4(sc, RGE_RSS_CTRL, 0);

	val = RGE_READ_2(sc, RGE_RXQUEUE_CTRL) & ~0x001c;
	RGE_WRITE_2(sc, RGE_RXQUEUE_CTRL, val | (1 - 1) << 2);

	RGE_CLRBIT_1(sc, RGE_CFG1, RGE_CFG1_SPEED_DOWN);

	rge_write_mac_ocp(sc, 0xc140, 0xffff);
	rge_write_mac_ocp(sc, 0xc142, 0xffff);

	RGE_MAC_SETBIT(sc, 0xeb58, 0x0001);

	if (RGE_TYPE_R26(sc) || RGE_TYPE_R27(sc)) {
		RGE_CLRBIT_1(sc, 0xd8, 0x02);
	}

	val = rge_read_mac_ocp(sc, 0xe614);
	val &= RGE_TYPE_R27(sc) ? ~0x0f00 : ~0x0700;
	if (sc->rge_type == MAC_R25 || RGE_TYPE_R25D(sc))
		rge_write_mac_ocp(sc, 0xe614, val | 0x0300);
	else if (sc->rge_type == MAC_R25B)
		rge_write_mac_ocp(sc, 0xe614, val | 0x0200);
	else if (RGE_TYPE_R26(sc))
		rge_write_mac_ocp(sc, 0xe614, val | 0x0300);
	else
		rge_write_mac_ocp(sc, 0xe614, val | 0x0f00);

	val = rge_read_mac_ocp(sc, 0xe63e) & ~0x0c00;
	rge_write_mac_ocp(sc, 0xe63e, val |
	    ((1 - 1) & 0x03) << 10);

	val = rge_read_mac_ocp(sc, 0xe63e) & ~0x0030;
	rge_write_mac_ocp(sc, 0xe63e, val | 0x0020);

	RGE_MAC_CLRBIT(sc, 0xc0b4, 0x0001);
	RGE_MAC_SETBIT(sc, 0xc0b4, 0x0001);

	RGE_MAC_SETBIT(sc, 0xc0b4, 0x000c);

	val = rge_read_mac_ocp(sc, 0xeb6a) & ~0x00ff;
	rge_write_mac_ocp(sc, 0xeb6a, val | 0x0033);

	val = rge_read_mac_ocp(sc, 0xeb50) & ~0x03e0;
	rge_write_mac_ocp(sc, 0xeb50, val | 0x0040);

	RGE_MAC_CLRBIT(sc, 0xe056, 0x00f0);

	RGE_WRITE_1(sc, RGE_TDFNR, 0x10);

	RGE_MAC_CLRBIT(sc, 0xe040, 0x1000);

	val = rge_read_mac_ocp(sc, 0xea1c) & ~0x0003;
	rge_write_mac_ocp(sc, 0xea1c, val | 0x0001);

	if (RGE_TYPE_R25D(sc))
		rge_write_mac_ocp(sc, 0xe0c0, 0x4403);
	else
		rge_write_mac_ocp(sc, 0xe0c0, 0x4000);

	RGE_MAC_SETBIT(sc, 0xe052, 0x0060);
	RGE_MAC_CLRBIT(sc, 0xe052, 0x0088);

	val = rge_read_mac_ocp(sc, 0xd430) & ~0x0fff;
	rge_write_mac_ocp(sc, 0xd430, val | 0x045f);

	RGE_SETBIT_1(sc, RGE_DLLPR, RGE_DLLPR_PFM_EN | RGE_DLLPR_TX_10M_PS_EN);
	RGE_SETBIT_1(sc, RGE_PPSW, 0x40);

	if (sc->rge_type == MAC_R25)
		RGE_SETBIT_1(sc, RGE_MCUCMD, 0x01);

	if (!RGE_TYPE_R25D(sc)) {
		RGE_MAC_CLRBIT(sc, 0xe080, 0x0002);
	}

	if (RGE_TYPE_R26(sc) || RGE_TYPE_R27(sc))
		RGE_MAC_CLRBIT(sc, 0xea1c, 0x0304);
	else
		RGE_MAC_CLRBIT(sc, 0xea1c, 0x0004);

	RGE_MAC_SETBIT(sc, 0xeb54, 0x0001);
	DELAY(1);
	RGE_MAC_CLRBIT(sc, 0xeb54, 0x0001);

	RGE_CLRBIT_2(sc, 0x1880, 0x0030);

	if (sc->rge_type != MAC_R25)
		RGE_CLRBIT_1(sc, RGE_INT_CFG0, RGE_INT_CFG0_EN);

	RGE_WRITE_4(sc, RGE_TIMERINT0, 0);
	RGE_WRITE_4(sc, RGE_TIMERINT1, 0);
	RGE_WRITE_4(sc, RGE_TIMERINT2, 0);
	RGE_WRITE_4(sc, RGE_TIMERINT3, 0);

	num_miti =
	    (sc->rge_type == MAC_R25B || RGE_TYPE_R26(sc)) ? 32 : 64;
	for (i = 0; i < num_miti; i++)
		RGE_WRITE_4(sc, RGE_INTMITI(i), 0);

	if (RGE_TYPE_R26(sc)) {
		RGE_CLRBIT_1(sc, RGE_INT_CFG0,
		    RGE_INT_CFG0_TIMEOUT_BYPASS | RGE_INT_CFG0_RDU_BYPASS_8126 |
		    RGE_INT_CFG0_MITIGATION_BYPASS);
		RGE_WRITE_2(sc, RGE_INT_CFG1, 0);
	}

	RGE_MAC_SETBIT(sc, 0xc0ac, 0x1f80);

	rge_write_mac_ocp(sc, 0xe098, 0xc302);

	RGE_MAC_CLRBIT(sc, 0xe032, 0x0003);
	val = rge_read_csi(sc, 0x98) & ~0x0000ff00;
	rge_write_csi(sc, 0x98, val);
	DELAY(20);

	if (RGE_TYPE_R25D(sc)) {
		val = rge_read_mac_ocp(sc, 0xe092) & ~0x00ff;
		rge_write_mac_ocp(sc, 0xe092, val | 0x0008);
	} else
		RGE_MAC_CLRBIT(sc, 0xe092, 0x00ff);

	RGE_SETBIT_2(sc, RGE_CPLUSCMD, RGE_CPLUSCMD_RXCSUM);
	RGE_READ_2(sc, RGE_CPLUSCMD);

	if (sc->rge_type == MAC_R25 || sc->rge_type == MAC_R25B)
		RGE_WRITE_2(sc, RGE_RXMAXSIZE, SLOT);
	else
		RGE_WRITE_2(sc, RGE_RXMAXSIZE,
		    SLOT | RGE_RXCFG_VLANPHYS);

	RGE_CLRBIT_1(sc, RGE_PPSW, 0x08);

	/* rge_iff() with every multicast group. */
	{
		uint32_t rxfilt = RGE_READ_4(sc, RGE_RXCFG);
		rxfilt &= ~(RGE_RXCFG_ALLPHYS | RGE_RXCFG_MULTI);
		rxfilt |= RGE_RXCFG_INDIV | RGE_RXCFG_BROAD;
		rxfilt |= RGE_RXCFG_MULTI;
		RGE_WRITE_4(sc, RGE_RXCFG, rxfilt);
		RGE_WRITE_4(sc, RGE_MAR0, 0xffffffff);
		RGE_WRITE_4(sc, RGE_MAR4, 0xffffffff);
	}

	rge_disable_aspm_clkreq(sc);

	RGE_CLRBIT_1(sc, RGE_EECMD, RGE_EECMD_WRITECFG);
	DELAY(10);

	/* rge_ifmedia_upd() for IFM_AUTO. */
	{
		int anar, gig;
		RGE_PHY_CLRBIT(sc, 0xa428, 0x0200);
		if (RGE_TYPE_R26(sc) || RGE_TYPE_R27(sc))
			RGE_PHY_CLRBIT(sc, 0xa5ea, 0x0007);
		else
			RGE_PHY_CLRBIT(sc, 0xa5ea, 0x0001);
		val = rge_read_phy_ocp(sc, 0xa5d4);
		switch (sc->rge_type) {
		case MAC_R26_1:
		case MAC_R26_2:
			val &= ~RGE_ADV_5000TFDX;
			/* fallthrough */
		default:
			val &= ~RGE_ADV_2500TFDX;
			break;
		}
		anar = ANAR_TX_FD | ANAR_TX | ANAR_10_FD | ANAR_10;
		gig = GTCR_ADV_1000TFDX | GTCR_ADV_1000THDX;
		val |= RGE_ADV_2500TFDX;
		if (RGE_TYPE_R26(sc))
			val |= RGE_ADV_5000TFDX;
		rge_write_phy(sc, 0, MII_ANAR, anar | ANAR_PAUSE_ASYM | ANAR_FC);
		rge_write_phy(sc, 0, MII_100T2CR, gig);
		rge_write_phy_ocp(sc, 0xa5d4, val);
		rge_write_phy(sc, 0, MII_BMCR, BMCR_RESET | BMCR_AUTOEN |
		    BMCR_STARTNEG);
	}

	RGE_WRITE_1(sc, RGE_CMD, RGE_CMD_TXENB | RGE_CMD_RXENB);

	/* rge_setup_intr(sc, RGE_IMTYPE_NONE) */
	RGE_WRITE_4(sc, RGE_IMR, RGE_INTRS);
	RGE_WRITE_4(sc, RGE_TIMERINT0, 0);
	RGE_WRITE_2(sc, RGE_IM, 0);
}

int main(int argc, char **argv) {
	static const struct { const char *name; enum rge_mac_type type; uint32_t rev; } kinds[] = {
		{ "r25", MAC_R25, 0x60900000 }, { "r25b", MAC_R25B, 0x64100000 },
		{ "r25d1", MAC_R25D_1, 0x68800000 }, { "r25d2", MAC_R25D_2, 0x68900000 },
		{ "r26_1", MAC_R26_1, 0x64900000 }, { "r26_2", MAC_R26_2, 0x64a00000 },
	};
	const uint8_t addr0[6] = { 0x00, 0xe0, 0x4c, 0x68, 0x12, 0x34 };
	for (unsigned k = 0; k < nitems(kinds); k++) {
		char path[256];
		snprintf(path, sizeof(path), "%s/c_%s.log", argc > 1 ? argv[1] : ".", kinds[k].name);
		out = fopen(path, "w");
		memset(regs, 0, sizeof(regs)); memset(mac, 0, sizeof(mac)); memset(phy, 0, sizeof(phy));
		memset(ephy, 0, sizeof(ephy)); memset(csi, 0, sizeof(csi));
		revision = kinds[k].rev;
		/* Every register starts with something in it, so that a mask
		 * read wrong shows. */
		for (uint32_t i = 0; i < sizeof(regs); i++) regs[i] = (uint8_t)((i * 0x9E3779B1u) >> 24);
		for (uint32_t i = 0; i < 0x8000; i++) mac[i] = (uint16_t)((i * 0x85EBCA6Bu) >> 16);
		for (uint32_t i = 0; i < 0x8000; i++) phy[i] = (uint16_t)((i * 0xC2B2AE35u) >> 16);
		for (uint32_t i = 0; i < 0x80; i++) ephy[i] = (uint16_t)((i * 0x27D4EB2Fu) >> 16);
		for (uint32_t i = 0; i < 0x400; i++) csi[i] = i * 0x165667B1u;
		phy[0xa420 >> 1] = 3;
		put(RGE_ADDR0, 4, addr0[0] | addr0[1] << 8 | addr0[2] << 16 | (uint32_t)addr0[3] << 24);
		put(RGE_ADDR1, 2, addr0[4] | addr0[5] << 8);
		csi[(0x70 + 0x10) >> 2] = 0x00110143;
		struct rge_softc s = { 0 }, *sc = &s;
		sc->rge_type = kinds[k].type;
		/* The port's start(): rge_attach()'s TXCFG read, ASPM off, address. */
		(void)RGE_READ_4(sc, RGE_TXCFG);
		{
			uint32_t reg = rge_read_csi(sc, 0x70 + 0x10);
			rge_write_csi(sc, 0x70 + 0x10, reg & ~(0x1 | 0x2 | 0x100));
		}
		uint8_t eaddr[6];
		uint32_t lo = RGE_READ_4(sc, RGE_ADDR0);
		uint16_t hi = RGE_READ_2(sc, RGE_ADDR1);
		memcpy(eaddr, &lo, 4); memcpy(eaddr + 4, &hi, 2);
		rge_set_macaddr(sc, eaddr);
		if (rge_chipinit(sc)) return 1;
		/* rge_init(): rge_stop(), the address, rge_chipinit(), the PHY. */
		RGE_CLRBIT_4(sc, RGE_RXCFG, RGE_RXCFG_ALLPHYS | RGE_RXCFG_INDIV |
		    RGE_RXCFG_MULTI | RGE_RXCFG_BROAD | RGE_RXCFG_RUNT |
		    RGE_RXCFG_ERRPKT);
		rge_hw_reset(sc);
		RGE_MAC_CLRBIT(sc, 0xc0ac, 0x1f80);
		rge_set_macaddr(sc, eaddr);
		if (rge_chipinit(sc)) return 1;
		if (rge_phy_config(sc)) return 1;
		init_rest(sc);
		fclose(out);
	}
	return 0;
}
