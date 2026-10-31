<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0257. Sessions use the network, and elevated commands set it up

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 13b.7. Before sign-in, root's shells came from consoled, whose
service file asks Spark for the network: to connect out, to listen on
any port, and to set the interfaces up with ip(8). Since people sign in
(ADR 0250), sessions come from authd, which held no network capability,
so a signed-in person reached nothing over the network, and an
administrator's command inside elevate (ADR 0219) could not set a card
up. Phase 12's network needs both: people use it, and administrators,
who are no longer root, set it up through elevate.

Linux lets anyone connect out and listen on the ports from 1024
(`net.ipv4.ip_unprivileged_port_start`); binding a lower port, and
setting up interfaces, need root or CAP_NET_BIND_SERVICE and
CAP_NET_ADMIN.

## Decision

- **authd's service file asks for the network** as consoled's does:
  `network.outbound`, `network.listen = ["1-65535"]` and
  `admin.network`. Spark gives these as separate capabilities in the
  network slots (ADRs 0183 and 0184).
- **A session gets the network as anyone has it on Linux.** authd gives
  each session two capabilities restricted from its own: one with the
  send right, to connect out, and one with the receive right over the
  ports from 1024 to 65535, to listen. The interfaces' setup (the
  control right) is not given, and binding a port below 1024 fails with
  EACCES, as on Linux. Both keep the copy and transfer rights, so they
  last through fork and exec.
- **Root's sessions, and commands run with elevate, get the network as
  authd holds it**: every port and the setup, as root's shell had from
  consoled. So `elevate ip link set net0 up` works, and an elevated
  server listens on port 80.

## Consequences

- People use the network from their sessions, and their programs
  listen on high ports, without elevate.
- Network administration is an administrator's, through elevate, as
  Chase's design has it; services that set the network up ask for
  `admin.network` themselves, as before.
- Apps from bundles get the network only as their permissions say
  (Phase 13d), from the session's or Spark's.
- **Not yet:** a per-person setting for low ports, as Linux's sysctl or
  CAP_NET_BIND_SERVICE per program; the firewall (Phase 12c) applies to
  sessions as to everyone.
