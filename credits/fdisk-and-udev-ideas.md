<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

Section: Projects whose ideas we follow
- [util-linux](https://github.com/util-linux/util-linux)'s fdisk: the
  single-letter commands, prompts and table columns of rdisk's
  interactive mode, kept so that the habit carries over
  ([ADR 0300](docs/adr/0300-one-disk-engine-for-rdisk-the-mkfs-tools-and-ob-image.md)).
- [systemd](https://systemd.io/)'s udev: the `/dev/disk/by-uuid`,
  `by-label`, `by-partuuid` and `by-partlabel` links and their `\xNN`
  escapes ([ADR 0303](docs/adr/0303-links-to-disks-by-label-and-uuid-in-devices-disk.md)).
- [dosfstools](https://github.com/dosfstools/dosfstools) and
  [exfatprogs](https://github.com/exfatprogs/exfatprogs): the options of
  `mkfs.fat`, `fsck.fat` and `mkfs.exfat`, which mkfs-fat, fsck-fat and
  mkfs-exfat take
  ([ADR 0302](docs/adr/0302-fat-and-exfat-made-and-checked-by-dukovos-own-tools.md)).
