# 0007. Closed firmware ships only as a separate optional package

- Status: Accepted
- Date: 2026-10-20

## Context

Some hardware needs closed firmware blobs: CPU microcode updates and the
Qualcomm QCA9377 Wi-Fi in the reference laptop, for example. The project
prefers open source.

## Decision

The base system contains no closed firmware. Firmware blobs are packaged
separately as an optional package installed outside `/system`, which users
add deliberately.

## Consequences

- The default image and base system stay fully open source and freely
  redistributable.
- Early microcode loading (roadmap Phase 1) works only when the optional
  package is installed.
- OrthrOS 1.0 supports wired Ethernet only; Wi-Fi would need the firmware
  package and a driver, and is outside the 1.0 scope.
