<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Networking

This chapter describes networking in Dukovo as of Phase 12a.1 of the
roadmap. Phase 12 builds it up in steps: sockets for programs, TCP
that matches Linux, `netd` with DHCP, DNS and `curl`, the time service,
the Realtek, Intel and USB Ethernet cards, IPv6, the firewall and
WireGuard. This page grows with each.

## What works now

- **virtio network cards**, the ones QEMU and clouds on KVM give a
  virtual machine, up to eight of them. The PCI scan finds each one and
  gives it a kernel thread, `net0`, `net1` and on.
- **The TCP/IP stack runs in the kernel**, built on
  [smoltcp](https://github.com/smoltcp-rs/smoltcp) in safe Rust
  ([ADR 0180](https://github.com/openlinuxsystems/orthros/blob/main/docs/adr/0180-the-tcp-ip-stack-runs-in-the-kernel-on-smoltcp.md)).
  For now it speaks Ethernet, ARP, IPv4 and ICMP: it answers pings, and
  the kernel's self-test pings with it.
- **Each card reaches only its own memory** behind the IOMMU: the
  driver copies frames through slots of DMA memory that belong to the
  card
  ([ADR 0181](https://github.com/openlinuxsystems/orthros/blob/main/docs/adr/0181-virtio-net-copies-frames-through-slots-of-its-own.md)).

Programs cannot open sockets yet, and nothing configures the network
by itself: there is no DHCP until `netd` lands.

## Giving a card an address

The first card takes its address from two kernel flags
(dukovo-boot(7)):

```
ip=10.0.2.15/24 gateway=10.0.2.2
```

Add them to `boot.cmdline` in `dukovo.toml`, or pass them with
`./ob run --cmdline`. QEMU's user network, which `ob run` does not add
by itself, hands out 10.0.2.15 and routes through 10.0.2.2. To try it:

```
./ob run --cmdline "selftest=net" -- -netdev user,id=n0 \
    -device virtio-net-pci,netdev=n0,disable-legacy=on
```

The log shows the card, its address and four pings to the router:

```
Net: net0 is a virtio network card at 00:03.0, MAC 52:54:00:12:34:56, 256 receive and 256 transmit slots, MSI-X vector 0x31
Net: net0 has the address 10.0.2.15/24, through the router 10.0.2.2
Net: 10.0.2.2 answered ping 1 in 0.412 ms
selftest net: passed; the router answered 4 pings, the fastest in 0.198 ms
```

`ob test --case net` runs the same test behind QEMU's IOMMU, as CI
does.
