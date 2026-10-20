# 0015. The framebuffer console uses the Spleen fonts

- Status: Accepted
- Date: 2026-10-20

## Context

The framebuffer console draws text itself, so the kernel needs a bitmap
font compiled in. The reference laptop has a 1920x1080 screen at 15.6
inches, where an 8x16 font is small; smaller screens, such as QEMU's
1280x800, need a small one. The font must be legible, cover ASCII and
box-drawing characters, come in several sizes, and have a license
compatible with Apache-2.0 OR MIT distribution (ADR 0002).

Candidates: the Linux console fonts (GPL, which ADR 0002 keeps out of the
kernel), Terminus (SIL Open Font License, which is permissive but has
reserved-name and bundling conditions), GNU Unifont (GPL with a font
exception, and only 8x16 and 16x16), and Spleen by Frederic Cambus
(BSD-2-Clause, PSF files from 5x8 to 32x64).

## Decision

The kernel embeds Spleen 2.1.0 in three sizes, 8x16, 12x24 and 16x32, as
the PSF files from the upstream release (PSF1 for 8x16, PSF2 for the
others), in `kernel/lib/fbcon/fonts/` with their license. The console
picks the largest size that still gives at least 100 columns and 40 rows
(12x24 on 1920x1080), and the `font` kernel flag overrides that.

## Consequences

- The fonts add about 72 KiB to the kernel image.
- BSD-2-Clause requires the copyright notice to accompany binary copies,
  so every image built by `ob image` carries `/licenses/LICENSE-spleen`,
  and the kernel crate's manifest points to it.
- The boot tests read the screen back by matching cells against the same
  font files, so a font change only needs the tests rebuilt, not updated.
- Revisit for non-Latin scripts, which Spleen does not cover; a console
  with full Unicode belongs to consoled in userspace (Phase 9).
