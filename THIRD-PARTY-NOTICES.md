# Third-Party Notices — Mimint

> Source: `cargo metadata --format-version 1` run on vega against
> the workspace `Cargo.lock` at commit `dd883e0` (Rust 1.98.1),
> 2026-11-19. Every package in the resolve is listed below with its
> declared `license` field exactly as reported by cargo metadata,
> plus its repository URL and a verdict.
>
> Packages in resolve: **495** (485 clean third-party, 5 workspace
> own-code, 5 noted with explanations below, **0 need review,
> 0 forbidden**).
>
> This table is a re-run of the Phase 4 license audit
> (`phase4/LICENSE-AUDIT.md`, 2026-11-18, 495 packages). Diffed
> against that audit: **0 packages added, 0 removed, 0 license-field
> changes** — every third-party row is byte-identical to Phase 4.
> Only the 5 own-code rows changed verdict text (repo license was
> TBD then; it is Apache 2.0 now, P8.0).
>
> Verdict rules: clean = declared license consists only of MIT /
> Apache-2.0 / BSD (1/2/3-Clause) / ISC / Zlib / Unlicense /
> Unicode-3.0 / Unicode-DFS-2016 / OpenSSL / CC0-1.0 / BSL-1.0
> (SPDX OR/AND combos counted clean when every alternative is in
> that set; the LLVM-exception is an additional permission on
> Apache-2.0, not a restriction). Any GPL / AGPL / LGPL term that
> actually applies would be FORBIDDEN and stop the build; none
> applies — the only GPL/LGPL tokens in the resolve sit behind ORs
> and are elected away (see the self_cell and r-efi notes).

## 1. Notes on specific packages

- `self_cell` 1.3.0 declares `Apache-2.0 OR GPL-2.0-only`. The OR
  means either may be elected; this project elects Apache-2.0, so no
  GPL term applies. Re-verified 2026-11-18: `self_cell` IS compiled
  into the Linux binaries (via `epaint` <- `egui` <- `eframe` <-
  `mimint-ui`, `cargo tree --target x86_64-unknown-linux-gnu -i
  self_cell`). The election is recorded here explicitly rather than
  hidden.
- `r-efi` 5.3.0 and 6.0.0 declare `MIT OR Apache-2.0 OR
  LGPL-2.1-or-later`. The OR means any one of the three may be
  elected; this project elects MIT / Apache-2.0, so no LGPL term
  applies. Re-verified 2026-11-18: `r-efi` appears 0 times in `cargo
  tree` for x86_64-unknown-linux-gnu. Both versions are present in
  Cargo.lock / the resolve only.
- `epaint_default_fonts` 0.37.0 declares `(MIT OR Apache-2.0) AND
  OFL-1.1 AND Ubuntu-font-1.0`: the code is MIT/Apache-2.0, and the
  bundled font data is under the SIL Open Font License 1.1 and the
  Ubuntu Font License 1.0. Both are permissive font licenses that
  allow redistribution with their notices kept. The font data IS
  compiled into the Linux binaries (via `epaint`). See section 2
  for the full packaging obligation. Not forbidden.
- `webpki-roots` 1.0.12 declares `CDLA-Permissive-2.0`, the
  permissive data license of the Mozilla root-certificate list it
  bundles. It IS compiled into the Linux binaries (via
  `rustls-tls`). Attribution (completed in P8.0, F8-09): the
  bundled data is Mozilla's root certificates, taken from the
  Common CA Database (CCADB, https://www.ccadb.org/) — the data in
  this crate is a derived work of the CCADB data, used under the
  CCADB Data Usage Terms. CDLA-Permissive-2.0 §2.1 requires the
  text of the agreement to be made available with the shared Data,
  so the full agreement text is reproduced in section 4 of this
  file and must be kept in binary distributions alongside the font
  notices in section 2.
- 18 crates declare `Unicode-3.0` (Unicode data files, e.g.
  character tables used by text layout). The Unicode License
  Agreement for data files is a permissive license with a notice
  obligation; counted clean under the verdict rules above.
- Workspace members `mimint-core`, `mimint-cli`, `mimint-body`,
  `mimint-ui`, and `mimint-control` are own clean-room code under
  the repository license (Apache License 2.0, see `LICENSE`).
  Their `license` fields in `Cargo.toml` still read `(none)` (four
  crates) / `MIT OR Apache-2.0` (`mimint-control`) — the fields
  were not changed in this docs-only phase; setting them to
  `Apache-2.0` is a follow-up for the next code commit.
- The macOS-only dependencies (`core-graphics`, `core-foundation`,
  `security-framework`) appear in the resolve for all targets but
  are compiled only for Apple targets; their licenses are MIT OR
  Apache-2.0. The Windows-only dependencies (`windows` and its
  `windows-*` companions) likewise appear in the resolve for all
  targets but are compiled only for Windows targets (verified by
  the `x86_64-pc-windows-gnu` check, 2026-11-18); all MIT OR
  Apache-2.0.

## 2. Font notices — obligation (from Phase 3, fulfilled here)

`epaint_default_fonts` 0.37.0 bundles these font files (checked
in the vega cargo registry copy of the crate,
`epaint_default_fonts-0.37.0/fonts/`):

| Font file in the crate | License | In the Linux binary? |
|---|---|---|
| `Hack-Regular.ttf` | SIL Open Font License 1.1 | Yes — included unconditionally |
| `Ubuntu-Light.ttf` | Ubuntu Font Licence 1.0 | Yes — included unconditionally |
| `NotoEmoji-Regular.ttf` | SIL Open Font License 1.1 | No — only with the `monochrome_emoji_fonts` feature, which is NOT enabled anywhere in this workspace (verified: `cargo tree -e features -i epaint_default_fonts` shows no such feature; `grep -r monochrome_emoji_fonts crates` = 0 hits) |
| `emoji-icon-font.ttf` | MIT (John Slegers, 2014) | No — same feature gate as above |
| `egui-icons.ttf` | (subset of emoji-icon-font, see `egui-icons.txt`) | Yes — bundled unconditionally (3.5 kB) |

**Obligation: every binary distribution of the Mimint desktop app
MUST keep the font license notices below with the distributed
binary** (for example in this file shipped alongside the binary, or
in an About / Licences screen in the app). The OFL-1.1 requires
the copyright + license notice to accompany redistribution of the
font software; the Ubuntu Font Licence 1.0 requires its licence
text to accompany redistribution. The crate itself ships
`OFL.txt`, `UFL.txt`, `Hack-Regular.txt` and
`emoji-icon-font-mit-license.txt` next to the fonts; their contents
are reproduced verbatim below so the obligation is fulfilled by
this file alone.

### 2a. SIL Open Font License, Version 1.1 (Hack, Noto Emoji)

```
```

### 2b. Ubuntu Font Licence, Version 1.0 (Ubuntu Light)

```
-------------------------------
UBUNTU FONT LICENCE Version 1.0
-------------------------------

PREAMBLE
This licence allows the licensed fonts to be used, studied, modified and
redistributed freely. The fonts, including any derivative works, can be
bundled, embedded, and redistributed provided the terms of this licence
are met. The fonts and derivatives, however, cannot be released under
any other licence. The requirement for fonts to remain under this
licence does not require any document created using the fonts or their
derivatives to be published under this licence, as long as the primary
purpose of the document is not to be a vehicle for the distribution of
the fonts.

DEFINITIONS
"Font Software" refers to the set of files released by the Copyright
Holder(s) under this licence and clearly marked as such. This may
include source files, build scripts and documentation.

"Original Version" refers to the collection of Font Software components
as received under this licence.

"Modified Version" refers to any derivative made by adding to, deleting,
or substituting -- in part or in whole -- any of the components of the
Original Version, by changing formats or by porting the Font Software to
a new environment.

"Copyright Holder(s)" refers to all individuals and companies who have a
copyright ownership of the Font Software.

"Substantially Changed" refers to Modified Versions which can be easily
identified as dissimilar to the Font Software by users of the Font
Software comparing the Original Version with the Modified Version.

To "Propagate" a work means to do anything with it that, without
permission, would make you directly or secondarily liable for
infringement under applicable copyright law, except executing it on a
computer or modifying a private copy. Propagation includes copying,
distribution (with or without modification and with or without charging
a redistribution fee), making available to the public, and in some
countries other activities as well.

PERMISSION & CONDITIONS
This licence does not grant any rights under trademark law and all such
rights are reserved.

Permission is hereby granted, free of charge, to any person obtaining a
copy of the Font Software, to propagate the Font Software, subject to
the below conditions:

1) Each copy of the Font Software must contain the above copyright
notice and this licence. These can be included either as stand-alone
text files, human-readable headers or in the appropriate machine-
readable metadata fields within text or binary files as long as those
fields can be easily viewed by the user.

2) The font name complies with the following:
(a) The Original Version must retain its name, unmodified.
(b) Modified Versions which are Substantially Changed must be renamed to
avoid use of the name of the Original Version or similar names entirely.
(c) Modified Versions which are not Substantially Changed must be
renamed to both (i) retain the name of the Original Version and (ii) add
additional naming elements to distinguish the Modified Version from the
Original Version. The name of such Modified Versions must be the name of
the Original Version, with "derivative X" where X represents the name of
the new work, appended to that name.

3) The name(s) of the Copyright Holder(s) and any contributor to the
Font Software shall not be used to promote, endorse or advertise any
Modified Version, except (i) as required by this licence, (ii) to
acknowledge the contribution(s) of the Copyright Holder(s) or (iii) with
their explicit written permission.

4) The Font Software, modified or unmodified, in part or in whole, must
be distributed entirely under this licence, and must not be distributed
under any other licence. The requirement for fonts to remain under this
licence does not affect any document created using the Font Software,
except any version of the Font Software extracted from a document
created using the Font Software may only be distributed under this
licence.

TERMINATION
This licence becomes null and void if any of the above conditions are
not met.

DISCLAIMER
THE FONT SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO ANY WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT OF
COPYRIGHT, PATENT, TRADEMARK, OR OTHER RIGHT. IN NO EVENT SHALL THE
COPYRIGHT HOLDER BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
INCLUDING ANY GENERAL, SPECIAL, INDIRECT, INCIDENTAL, OR CONSEQUENTIAL
DAMAGES, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF THE USE OR INABILITY TO USE THE FONT SOFTWARE OR FROM OTHER
DEALINGS IN THE FONT SOFTWARE.
```

### 2c. MIT — emoji-icon-font (John Slegers, 2014)

```
MIT License

Copyright (c) 2014 John Slegers

Permission is hereby granted, free of charge, to any person obtaining a copy of this software and associated documentation files (the "Software"), to deal in the Software without restriction, including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
```

### 2d. MIT + Bitstream Vera notice — Hack (Source Foundry, 2018)

```
The work in the Hack project is Copyright 2018 Source Foundry Authors and licensed under the MIT License

The work in the DejaVu project was committed to the public domain.

Bitstream Vera Sans Mono Copyright 2003 Bitstream Inc. and licensed under the Bitstream Vera License with Reserved Font Names "Bitstream" and "Vera"
MIT License

Copyright (c) 2018 Source Foundry Authors

Permission is hereby granted, free of charge, to any person obtaining a copy of this software and associated documentation files (the "Software"), to deal in the Software without restriction, including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
BITSTREAM VERA LICENSE

Copyright (c) 2003 by Bitstream, Inc. All Rights Reserved. Bitstream Vera is a trademark of Bitstream, Inc.

Permission is hereby granted, free of charge, to any person obtaining a copy of the fonts accompanying this license ("Fonts") and associated documentation files (the "Font Software"), to reproduce and distribute the Font Software, including without limitation the rights to use, copy, merge, publish, distribute, and/or sell copies of the Font Software, and to permit persons to whom the Font Software is furnished to do so, subject to the following conditions:

The above copyright and trademark notices and this permission notice shall be included in all copies of one or more of the Font Software typefaces.

The Font Software may be modified, altered, or added to, and in particular the designs of glyphs or characters in the Fonts may be modified and additional glyphs or characters may be added to the Fonts, only if the fonts are renamed to names not containing either the words "Bitstream" or the word "Vera".

This License becomes null and void to the extent applicable to Fonts or Font Software that has been modified and is distributed under the "Bitstream Vera" names.

The Font Software may be sold as part of a larger software package but no copy of one or more of the Font Software typefaces may be sold by itself.

THE FONT SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO ANY WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT OF COPYRIGHT, PATENT, TRADEMARK, OR OTHER RIGHT. IN NO EVENT SHALL BITSTREAM OR THE GNOME FOUNDATION BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, INCLUDING ANY GENERAL, SPECIAL, INDIRECT, INCIDENTAL, OR CONSEQUENTIAL DAMAGES, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF THE USE OR INABILITY TO USE THE FONT SOFTWARE OR FROM OTHER DEALINGS IN THE FONT SOFTWARE.

Except as contained in this notice, the names of Gnome, the Gnome Foundation, and Bitstream Inc., shall not be used in advertising or otherwise to promote the sale, use or other dealings in this Font Software without prior written authorization from the Gnome Foundation or Bitstream Inc., respectively. For further information, contact: fonts at gnome dot org.
```

## 3. Full dependency table

| crate | version | license | repository | verdict |
|---|---|---|---|---|
| ab_glyph | 0.2.32 | Apache-2.0 | https://github.com/alexheretic/ab-glyph | clean |
| ab_glyph_rasterizer | 0.1.10 | Apache-2.0 | https://github.com/alexheretic/ab-glyph | clean |
| accesskit | 0.25.2 | MIT OR Apache-2.0 | https://github.com/AccessKit/accesskit | clean |
| accesskit_atspi_common | 0.21.1 | MIT OR Apache-2.0 | https://github.com/AccessKit/accesskit | clean |
| accesskit_consumer | 0.39.2 | MIT OR Apache-2.0 | https://github.com/AccessKit/accesskit | clean |
| accesskit_ios | 0.2.2 | MIT OR Apache-2.0 | https://github.com/AccessKit/accesskit | clean |
| accesskit_macos | 0.27.2 | MIT OR Apache-2.0 | https://github.com/AccessKit/accesskit | clean |
| accesskit_unix | 0.24.1 | MIT OR Apache-2.0 | https://github.com/AccessKit/accesskit | clean |
| accesskit_windows | 0.35.2 | MIT OR Apache-2.0 | https://github.com/AccessKit/accesskit | clean |
| accesskit_winit | 0.34.2 | Apache-2.0 | https://github.com/AccessKit/accesskit | clean |
| adler2 | 2.0.1 | 0BSD OR MIT OR Apache-2.0 | https://github.com/oyvindln/adler2 | clean |
| ahash | 0.8.12 | MIT OR Apache-2.0 | https://github.com/tkaitchuck/ahash | clean |
| allocator-api2 | 0.2.21 | MIT OR Apache-2.0 | https://github.com/zakarumych/allocator-api2 | clean |
| android-activity | 0.6.1 | MIT OR Apache-2.0 | https://github.com/rust-mobile/android-activity | clean |
| android-properties | 0.2.2 | MIT | https://github.com/miklelappo/android-properties | clean |
| android_system_properties | 0.1.6 | MIT OR Apache-2.0 | https://github.com/nical/android_system_properties | clean |
| arboard | 3.7.0 | MIT OR Apache-2.0 | https://github.com/1Password/arboard | clean |
| arrayref | 0.3.9 | BSD-2-Clause | https://github.com/droundy/arrayref | clean |
| arrayvec | 0.7.8 | MIT OR Apache-2.0 | https://github.com/bluss/arrayvec | clean |
| as-raw-xcb-connection | 1.0.1 | MIT OR Apache-2.0 | https://github.com/psychon/as-raw-xcb-connection | clean |
| ash | 0.38.0+1.3.281 | MIT OR Apache-2.0 | https://github.com/ash-rs/ash | clean |
| async-broadcast | 0.7.2 | MIT OR Apache-2.0 | https://github.com/smol-rs/async-broadcast | clean |
| async-channel | 2.5.0 | Apache-2.0 OR MIT | https://github.com/smol-rs/async-channel | clean |
| async-executor | 1.14.1 | Apache-2.0 OR MIT | https://github.com/smol-rs/async-executor | clean |
| async-io | 2.6.0 | Apache-2.0 OR MIT | https://github.com/smol-rs/async-io | clean |
| async-lock | 3.5.0 | Apache-2.0 OR MIT | https://github.com/smol-rs/async-lock | clean |
| async-process | 2.5.0 | Apache-2.0 OR MIT | https://github.com/smol-rs/async-process | clean |
| async-recursion | 1.2.0 | MIT OR Apache-2.0 | https://github.com/dcchut/async-recursion | clean |
| async-signal | 0.2.14 | Apache-2.0 OR MIT | https://github.com/smol-rs/async-signal | clean |
| async-task | 4.7.1 | Apache-2.0 OR MIT | https://github.com/smol-rs/async-task | clean |
| async-trait | 0.1.92 | MIT OR Apache-2.0 | https://github.com/dtolnay/async-trait | clean |
| atomic-waker | 1.1.2 | Apache-2.0 OR MIT | https://github.com/smol-rs/atomic-waker | clean |
| atspi | 0.29.0 | Apache-2.0 OR MIT | https://github.com/odilia-app/atspi | clean |
| atspi-common | 0.13.0 | Apache-2.0 OR MIT | https://github.com/odilia-app/atspi | clean |
| atspi-proxies | 0.13.0 | Apache-2.0 OR MIT | https://github.com/odilia-app/atspi | clean |
| autocfg | 1.5.1 | Apache-2.0 OR MIT | https://github.com/cuviper/autocfg | clean |
| base64 | 0.22.1 | MIT OR Apache-2.0 | https://github.com/marshallpierce/rust-base64 | clean |
| base64 | 0.23.1 | MIT OR Apache-2.0 | https://github.com/marshallpierce/rust-base64 | clean |
| bit-set | 0.10.0 | Apache-2.0 OR MIT | https://github.com/contain-rs/bit-set | clean |
| bit-vec | 0.9.1 | Apache-2.0 OR MIT | https://github.com/contain-rs/bit-vec | clean |
| bitflags | 1.3.2 | MIT/Apache-2.0 | https://github.com/bitflags/bitflags | clean |
| bitflags | 2.13.2 | MIT OR Apache-2.0 | https://github.com/bitflags/bitflags | clean |
| block2 | 0.5.1 | MIT | https://github.com/madsmtm/objc2 | clean |
| block2 | 0.6.2 | MIT | https://github.com/madsmtm/objc2 | clean |
| blocking | 1.7.0 | Apache-2.0 OR MIT | https://github.com/smol-rs/blocking | clean |
| bumpalo | 3.20.4 | MIT OR Apache-2.0 | https://github.com/fitzgen/bumpalo | clean |
| bytemuck | 1.25.2 | Zlib OR Apache-2.0 OR MIT | https://github.com/Lokathor/bytemuck | clean |
| bytemuck_derive | 1.12.1 | Zlib OR Apache-2.0 OR MIT | https://github.com/Lokathor/bytemuck | clean |
| byteorder-lite | 0.1.0 | Unlicense OR MIT | https://github.com/image-rs/byteorder-lite | clean |
| bytes | 1.12.1 | MIT | https://github.com/tokio-rs/bytes | clean |
| calloop | 0.13.0 | MIT | https://github.com/Smithay/calloop | clean |
| calloop | 0.14.5 | MIT | https://github.com/Smithay/calloop | clean |
| calloop-wayland-source | 0.3.0 | MIT | https://github.com/smithay/calloop-wayland-source | clean |
| calloop-wayland-source | 0.4.1 | MIT | https://github.com/smithay/calloop-wayland-source | clean |
| cc | 1.11.2 | MIT OR Apache-2.0 | https://github.com/rust-lang/cc-rs | clean |
| cfg-if | 1.0.5 | MIT OR Apache-2.0 | https://github.com/rust-lang/cfg-if | clean |
| cfg_aliases | 0.2.2 | MIT | https://github.com/katharostech/cfg_aliases | clean |
| cgl | 0.3.2 | MIT / Apache-2.0 | https://github.com/servo/cgl-rs | clean |
| chacha20 | 0.10.2 | MIT OR Apache-2.0 | https://github.com/RustCrypto/stream-ciphers | clean |
| clipboard-win | 5.4.3 | BSL-1.0 | https://github.com/DoumanAsh/clipboard-win | clean |
| codespan-reporting | 0.13.1 | Apache-2.0 | https://github.com/brendanzab/codespan | clean |
| color | 0.3.3 | Apache-2.0 OR MIT | https://github.com/linebender/color | clean |
| combine | 4.6.9 | MIT | https://github.com/Marwes/combine | clean |
| concurrent-queue | 2.5.0 | Apache-2.0 OR MIT | https://github.com/smol-rs/concurrent-queue | clean |
| core-foundation | 0.10.1 | MIT OR Apache-2.0 | https://github.com/servo/core-foundation-rs | clean |
| core-foundation | 0.9.4 | MIT OR Apache-2.0 | https://github.com/servo/core-foundation-rs | clean |
| core-foundation-sys | 0.8.7 | MIT OR Apache-2.0 | https://github.com/servo/core-foundation-rs | clean |
| core-graphics | 0.23.2 | MIT OR Apache-2.0 | https://github.com/servo/core-foundation-rs | clean |
| core-graphics | 0.25.0 | MIT OR Apache-2.0 | https://github.com/servo/core-foundation-rs | clean |
| core-graphics-types | 0.1.3 | MIT OR Apache-2.0 | https://github.com/servo/core-foundation-rs | clean |
| core-graphics-types | 0.2.0 | MIT OR Apache-2.0 | https://github.com/servo/core-foundation-rs | clean |
| cpufeatures | 0.3.1 | MIT OR Apache-2.0 | https://github.com/RustCrypto/utils | clean |
| crc32fast | 1.5.5 | MIT OR Apache-2.0 | https://github.com/srijs/rust-crc32fast | clean |
| crossbeam-utils | 0.8.23 | MIT OR Apache-2.0 | https://github.com/crossbeam-rs/crossbeam | clean |
| crunchy | 0.2.4 | MIT | https://github.com/eira-fransham/crunchy | clean |
| cursor-icon | 1.2.0 | MIT OR Apache-2.0 OR Zlib | https://github.com/rust-windowing/cursor-icon | clean |
| dispatch | 0.2.0 | MIT | http://github.com/SSheldon/rust-dispatch | clean |
| dispatch2 | 0.3.1 | Zlib OR Apache-2.0 OR MIT | https://github.com/madsmtm/objc2 | clean |
| displaydoc | 0.2.7 | MIT OR Apache-2.0 | https://github.com/yaahc/displaydoc | clean |
| dlib | 0.5.3 | MIT | https://github.com/elinorbgr/dlib | clean |
| document-features | 0.2.12 | MIT OR Apache-2.0 | https://github.com/slint-ui/document-features | clean |
| downcast-rs | 1.2.1 | MIT/Apache-2.0 | https://github.com/marcianx/downcast-rs | clean |
| dpi | 0.1.2 | Apache-2.0 AND MIT | https://github.com/rust-windowing/winit | clean |
| ecolor | 0.37.0 | MIT OR Apache-2.0 | https://github.com/emilk/egui | clean |
| eframe | 0.37.0 | MIT OR Apache-2.0 | https://github.com/emilk/egui/tree/main/crates/eframe | clean |
| egui | 0.37.0 | MIT OR Apache-2.0 | https://github.com/emilk/egui | clean |
| egui-wgpu | 0.37.0 | MIT OR Apache-2.0 | https://github.com/emilk/egui/tree/main/crates/egui-wgpu | clean |
| egui-winit | 0.37.0 | MIT OR Apache-2.0 | https://github.com/emilk/egui/tree/main/crates/egui-winit | clean |
| egui_glow | 0.37.0 | MIT OR Apache-2.0 | https://github.com/emilk/egui/tree/main/crates/egui_glow | clean |
| egui_system_font_provider | 0.37.0 | MIT OR Apache-2.0 | https://github.com/emilk/egui/tree/main/crates/egui_system_font_provider | clean |
| either | 1.19.0 | MIT OR Apache-2.0 | https://github.com/rayon-rs/either | clean |
| emath | 0.37.0 | MIT OR Apache-2.0 | https://github.com/emilk/egui/tree/main/crates/emath | clean |
| endi | 1.1.1 | MIT | https://github.com/zeenix/endi | clean |
| enumflags2 | 0.7.12 | MIT OR Apache-2.0 | https://github.com/meithecatte/enumflags2 | clean |
| enumflags2_derive | 0.7.12 | MIT OR Apache-2.0 | https://github.com/meithecatte/enumflags2 | clean |
| epaint | 0.37.0 | MIT OR Apache-2.0 | https://github.com/emilk/egui/tree/main/crates/epaint | clean |
| epaint_default_fonts | 0.37.0 | (MIT OR Apache-2.0) AND OFL-1.1 AND Ubuntu-font-1.0 | https://github.com/emilk/egui/tree/main/crates/epaint_default_fonts | noted: font data under OFL-1.1 + Ubuntu-font-1.0; keep font notices on redistribution (see note) |
| equivalent | 1.0.3 | Apache-2.0 OR MIT | https://github.com/indexmap-rs/equivalent | clean |
| errno | 0.3.15 | MIT OR Apache-2.0 | https://github.com/lambda-fairy/rust-errno | clean |
| error-code | 3.4.0 | BSL-1.0 | https://github.com/DoumanAsh/error-code | clean |
| euclid | 0.22.14 | MIT OR Apache-2.0 | https://github.com/servo/euclid | clean |
| event-listener | 5.4.2 | Apache-2.0 OR MIT | https://github.com/smol-rs/event-listener | clean |
| event-listener-strategy | 0.5.4 | Apache-2.0 OR MIT | https://github.com/smol-rs/event-listener-strategy | clean |
| fallible-iterator | 0.3.0 | MIT/Apache-2.0 | https://github.com/sfackler/rust-fallible-iterator | clean |
| fallible-streaming-iterator | 0.1.9 | MIT/Apache-2.0 | https://github.com/sfackler/fallible-streaming-iterator | clean |
| fastrand | 2.5.0 | Apache-2.0 OR MIT | https://github.com/smol-rs/fastrand | clean |
| fax | 0.2.7 | MIT | https://github.com/pdf-rs/fax | clean |
| fdeflate | 0.3.7 | MIT OR Apache-2.0 | https://github.com/image-rs/fdeflate | clean |
| fearless_simd | 0.4.1 | Apache-2.0 OR MIT | https://github.com/linebender/fearless_simd | clean |
| find-msvc-tools | 0.1.15 | MIT OR Apache-2.0 | https://github.com/rust-lang/cc-rs | clean |
| fixedbitset | 0.5.9 | MIT OR Apache-2.0 | https://github.com/petgraph/fixedbitset | clean |
| flate2 | 1.1.11 | MIT OR Apache-2.0 | https://github.com/rust-lang/flate2-rs | clean |
| foldhash | 0.1.5 | Zlib | https://github.com/orlp/foldhash | clean |
| foldhash | 0.2.0 | Zlib | https://github.com/orlp/foldhash | clean |
| font-types | 0.12.7 | MIT OR Apache-2.0 | https://github.com/googlefonts/fontations | clean |
| fontique | 0.11.1 | Apache-2.0 OR MIT | https://github.com/linebender/parley | clean |
| foreign-types | 0.5.0 | MIT/Apache-2.0 | https://github.com/sfackler/foreign-types | clean |
| foreign-types-macros | 0.2.4 | MIT/Apache-2.0 | https://github.com/sfackler/foreign-types | clean |
| foreign-types-shared | 0.3.1 | MIT/Apache-2.0 | https://github.com/sfackler/foreign-types | clean |
| form_urlencoded | 1.2.2 | MIT OR Apache-2.0 | https://github.com/servo/rust-url | clean |
| futures-channel | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs | clean |
| futures-core | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs | clean |
| futures-io | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs | clean |
| futures-lite | 2.6.1 | Apache-2.0 OR MIT | https://github.com/smol-rs/futures-lite | clean |
| futures-macro | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs | clean |
| futures-task | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs | clean |
| futures-util | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs | clean |
| gethostname | 1.1.0 | Apache-2.0 | https://codeberg.org/swsnr/gethostname.rs.git | clean |
| getrandom | 0.2.17 | MIT OR Apache-2.0 | https://github.com/rust-random/getrandom | clean |
| getrandom | 0.3.4 | MIT OR Apache-2.0 | https://github.com/rust-random/getrandom | clean |
| getrandom | 0.4.3 | MIT OR Apache-2.0 | https://github.com/rust-random/getrandom | clean |
| gl_generator | 0.14.0 | Apache-2.0 | https://github.com/brendanzab/gl-rs/ | clean |
| glifo | 0.3.0 | Apache-2.0 OR MIT | https://github.com/linebender/vello | clean |
| glow | 0.17.0 | MIT OR Apache-2.0 OR Zlib | https://github.com/grovesNL/glow | clean |
| glutin | 0.32.3 | Apache-2.0 | https://github.com/rust-windowing/glutin | clean |
| glutin-winit | 0.5.0 | MIT | https://github.com/rust-windowing/glutin | clean |
| glutin_egl_sys | 0.7.1 | Apache-2.0 | https://github.com/rust-windowing/glutin | clean |
| glutin_glx_sys | 0.6.1 | Apache-2.0 | https://github.com/rust-windowing/glutin | clean |
| glutin_wgl_sys | 0.6.1 | Apache-2.0 | https://github.com/rust-windowing/glutin | clean |
| gpu-allocator | 0.28.0 | MIT OR Apache-2.0 | https://github.com/Traverse-Research/gpu-allocator | clean |
| guillotiere | 0.7.0 | MIT/Apache-2.0 | https://github.com/nical/guillotiere | clean |
| half | 2.7.1 | MIT OR Apache-2.0 | https://github.com/VoidStarKat/half-rs | clean |
| harfrust | 0.12.0 | MIT | https://github.com/harfbuzz/harfrust | clean |
| hashbrown | 0.14.5 | MIT OR Apache-2.0 | https://github.com/rust-lang/hashbrown | clean |
| hashbrown | 0.15.5 | MIT OR Apache-2.0 | https://github.com/rust-lang/hashbrown | clean |
| hashbrown | 0.16.1 | MIT OR Apache-2.0 | https://github.com/rust-lang/hashbrown | clean |
| hashbrown | 0.17.1 | MIT OR Apache-2.0 | https://github.com/rust-lang/hashbrown | clean |
| hashlink | 0.9.1 | MIT OR Apache-2.0 | https://github.com/kyren/hashlink | clean |
| hermit-abi | 0.5.3 | MIT OR Apache-2.0 | https://github.com/hermit-os/hermit-rs | clean |
| hex | 0.4.3 | MIT OR Apache-2.0 | https://github.com/KokaKiwi/rust-hex | clean |
| http | 1.5.0 | MIT OR Apache-2.0 | https://github.com/hyperium/http | clean |
| http-body | 1.1.0 | MIT | https://github.com/hyperium/http-body | clean |
| http-body-util | 0.1.5 | MIT | https://github.com/hyperium/http-body | clean |
| httparse | 1.10.1 | MIT OR Apache-2.0 | https://github.com/seanmonstar/httparse | clean |
| hyper | 1.12.0 | MIT | https://github.com/hyperium/hyper | clean |
| hyper-rustls | 0.27.10 | Apache-2.0 OR ISC OR MIT | https://github.com/rustls/hyper-rustls | clean |
| hyper-util | 0.1.21 | MIT | https://github.com/hyperium/hyper-util | clean |
| icu_collections | 2.4.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| icu_locale_core | 2.4.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| icu_normalizer | 2.4.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| icu_normalizer_data | 2.4.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| icu_properties | 2.4.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| icu_properties_data | 2.4.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| icu_provider | 2.4.0 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| idna | 1.1.0 | MIT OR Apache-2.0 | https://github.com/servo/rust-url/ | clean |
| idna_adapter | 1.2.2 | Apache-2.0 OR MIT | https://github.com/hsivonen/idna_adapter | clean |
| image | 0.25.10 | MIT OR Apache-2.0 | https://github.com/image-rs/image | clean |
| indexmap | 2.14.2 | Apache-2.0 OR MIT | https://github.com/indexmap-rs/indexmap | clean |
| ipnet | 2.12.2 | MIT OR Apache-2.0 | https://github.com/krisprice/ipnet | clean |
| itertools | 0.15.0 | MIT OR Apache-2.0 | https://github.com/rust-itertools/itertools | clean |
| itoa | 1.0.19 | MIT OR Apache-2.0 | https://github.com/dtolnay/itoa | clean |
| jni | 0.22.4 | MIT OR Apache-2.0 | https://github.com/jni-rs/jni-rs | clean |
| jni-macros | 0.22.4 | MIT OR Apache-2.0 | https://github.com/jni-rs/jni-rs | clean |
| jni-sys | 0.3.1 | MIT OR Apache-2.0 | https://github.com/jni-rs/jni-sys | clean |
| jni-sys | 0.4.1 | MIT OR Apache-2.0 | https://github.com/jni-rs/jni-sys | clean |
| jni-sys-macros | 0.4.1 | MIT OR Apache-2.0 | https://github.com/jni-rs/jni-sys | clean |
| jobserver | 0.1.35 | MIT OR Apache-2.0 | https://github.com/rust-lang/jobserver-rs | clean |
| js-sys | 0.3.107 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/js-sys | clean |
| khronos-egl | 6.0.0 | MIT/Apache-2.0 | https://github.com/timothee-haudebourg/khronos-egl | clean |
| khronos_api | 3.1.0 | Apache-2.0 | https://github.com/brendanzab/gl-rs/ | clean |
| kurbo | 0.13.1 | Apache-2.0 OR MIT | https://github.com/linebender/kurbo | clean |
| libc | 0.2.191 | MIT OR Apache-2.0 | https://github.com/rust-lang/libc | clean |
| libloading | 0.8.9 | ISC | https://github.com/nagisa/rust_libloading/ | clean |
| libm | 0.2.16 | MIT | https://github.com/rust-lang/compiler-builtins | clean |
| libredox | 0.1.25 | MIT | https://gitlab.redox-os.org/redox-os/libredox.git | clean |
| libsqlite3-sys | 0.30.1 | MIT | https://github.com/rusqlite/rusqlite | clean |
| linebender_resource_handle | 0.1.1 | Apache-2.0 OR MIT | https://github.com/linebender/raw_resource_handle | clean |
| linux-raw-sys | 0.12.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://github.com/sunfishcode/linux-raw-sys | clean |
| linux-raw-sys | 0.4.15 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://github.com/sunfishcode/linux-raw-sys | clean |
| litemap | 0.8.3 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| litrs | 1.0.1 | MIT OR Apache-2.0 | https://github.com/LukasKalbertodt/litrs | clean |
| lock_api | 0.4.14 | MIT OR Apache-2.0 | https://github.com/Amanieu/parking_lot | clean |
| log | 0.4.35 | MIT OR Apache-2.0 | https://github.com/rust-lang/log | clean |
| lru-slab | 0.1.3 | MIT OR Apache-2.0 OR Zlib | https://github.com/Ralith/lru-slab | clean |
| memchr | 2.8.3 | Unlicense OR MIT | https://github.com/BurntSushi/memchr | clean |
| memmap2 | 0.9.12 | MIT OR Apache-2.0 | https://github.com/RazrFalcon/memmap2-rs | clean |
| memoffset | 0.9.1 | MIT | https://github.com/Gilnaa/memoffset | clean |
| mimint-body | 0.1.0 | (none) | — | own code - Apache-2.0 (repo LICENSE, P8.0) |
| mimint-cli | 0.1.0 | (none) | — | own code - Apache-2.0 (repo LICENSE, P8.0) |
| mimint-control | 0.1.0 | MIT OR Apache-2.0 | — | own code - Apache-2.0 (repo LICENSE, P8.0) |
| mimint-core | 0.1.0 | (none) | — | own code - Apache-2.0 (repo LICENSE, P8.0) |
| mimint-ui | 0.1.0 | (none) | — | own code - Apache-2.0 (repo LICENSE, P8.0) |
| miniz_oxide | 0.8.9 | MIT OR Zlib OR Apache-2.0 | https://github.com/Frommi/miniz_oxide/tree/master/miniz_oxide | clean |
| miniz_oxide | 0.9.1 | MIT OR Zlib OR Apache-2.0 | https://github.com/Frommi/miniz_oxide/tree/master/miniz_oxide | clean |
| mio | 1.2.5 | MIT | https://github.com/tokio-rs/mio | clean |
| moxcms | 0.8.1 | BSD-3-Clause OR Apache-2.0 | https://github.com/awxkee/moxcms.git | clean |
| naga | 30.0.2 | MIT OR Apache-2.0 | https://github.com/gfx-rs/wgpu | clean |
| naga-types | 30.0.2 | MIT OR Apache-2.0 | https://github.com/gfx-rs/wgpu | clean |
| ndk | 0.9.0 | MIT OR Apache-2.0 | https://github.com/rust-mobile/ndk | clean |
| ndk-context | 0.1.1 | MIT OR Apache-2.0 | https://github.com/rust-windowing/android-ndk-rs | clean |
| ndk-sys | 0.6.0+11769913 | MIT OR Apache-2.0 | https://github.com/rust-mobile/ndk | clean |
| nohash-hasher | 0.2.0 | Apache-2.0 OR MIT | https://github.com/paritytech/nohash-hasher | clean |
| nom | 8.0.0 | MIT | https://github.com/rust-bakery/nom | clean |
| num-traits | 0.2.19 | MIT OR Apache-2.0 | https://github.com/rust-num/num-traits | clean |
| num_enum | 0.7.6 | BSD-3-Clause OR MIT OR Apache-2.0 | https://github.com/illicitonion/num_enum | clean |
| num_enum_derive | 0.7.6 | BSD-3-Clause OR MIT OR Apache-2.0 | https://github.com/illicitonion/num_enum | clean |
| objc-sys | 0.3.5 | MIT | https://github.com/madsmtm/objc2 | clean |
| objc2 | 0.5.3 | MIT | https://github.com/madsmtm/objc2 | clean |
| objc2 | 0.6.5 | MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-app-kit | 0.2.2 | MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-app-kit | 0.3.2 | Zlib OR Apache-2.0 OR MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-cloud-kit | 0.2.2 | MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-contacts | 0.2.2 | MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-core-data | 0.2.2 | MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-core-foundation | 0.3.2 | Zlib OR Apache-2.0 OR MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-core-graphics | 0.3.2 | Zlib OR Apache-2.0 OR MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-core-image | 0.2.2 | MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-core-location | 0.2.2 | MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-core-text | 0.3.2 | Zlib OR Apache-2.0 OR MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-encode | 4.1.0 | MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-foundation | 0.2.2 | MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-foundation | 0.3.2 | MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-io-surface | 0.3.2 | Zlib OR Apache-2.0 OR MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-link-presentation | 0.2.2 | MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-metal | 0.2.2 | MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-metal | 0.3.2 | Zlib OR Apache-2.0 OR MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-quartz-core | 0.2.2 | MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-quartz-core | 0.3.2 | Zlib OR Apache-2.0 OR MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-symbols | 0.2.2 | MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-ui-kit | 0.2.2 | MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-ui-kit | 0.3.2 | Zlib OR Apache-2.0 OR MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-uniform-type-identifiers | 0.2.2 | MIT | https://github.com/madsmtm/objc2 | clean |
| objc2-user-notifications | 0.2.2 | MIT | https://github.com/madsmtm/objc2 | clean |
| once_cell | 1.21.4 | MIT OR Apache-2.0 | https://github.com/matklad/once_cell | clean |
| orbclient | 0.3.55 | MIT | https://gitlab.redox-os.org/redox-os/orbclient | clean |
| ordered-float | 5.5.1 | MIT | https://github.com/reem/rust-ordered-float | clean |
| ordered-stream | 0.2.0 | MIT OR Apache-2.0 | https://github.com/danieldg/ordered-stream | clean |
| os_pipe | 1.2.3 | MIT | https://github.com/oconnor663/os_pipe.rs | clean |
| owned_ttf_parser | 0.25.1 | Apache-2.0 | https://github.com/alexheretic/owned-ttf-parser | clean |
| parking | 2.2.1 | Apache-2.0 OR MIT | https://github.com/smol-rs/parking | clean |
| parking_lot | 0.12.5 | MIT OR Apache-2.0 | https://github.com/Amanieu/parking_lot | clean |
| parking_lot_core | 0.9.12 | MIT OR Apache-2.0 | https://github.com/Amanieu/parking_lot | clean |
| parlance | 0.1.1 | Apache-2.0 OR MIT | https://github.com/linebender/parley | clean |
| peniko | 0.6.1 | Apache-2.0 OR MIT | https://github.com/linebender/peniko | clean |
| percent-encoding | 2.3.2 | MIT OR Apache-2.0 | https://github.com/servo/rust-url/ | clean |
| petgraph | 0.8.3 | MIT OR Apache-2.0 | https://github.com/petgraph/petgraph | clean |
| phf | 0.14.0 | MIT | https://github.com/rust-phf/rust-phf | clean |
| phf_generator | 0.14.0 | MIT | https://github.com/rust-phf/rust-phf | clean |
| phf_macros | 0.14.0 | MIT | https://github.com/rust-phf/rust-phf | clean |
| phf_shared | 0.14.0 | MIT | https://github.com/rust-phf/rust-phf | clean |
| pin-project | 1.1.13 | Apache-2.0 OR MIT | https://github.com/taiki-e/pin-project | clean |
| pin-project-internal | 1.1.13 | Apache-2.0 OR MIT | https://github.com/taiki-e/pin-project | clean |
| pin-project-lite | 0.2.17 | Apache-2.0 OR MIT | https://github.com/taiki-e/pin-project-lite | clean |
| piper | 0.2.5 | MIT OR Apache-2.0 | https://github.com/smol-rs/piper | clean |
| pkg-config | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/pkg-config-rs | clean |
| plain | 0.2.3 | MIT/Apache-2.0 | https://github.com/randomites/plain | clean |
| png | 0.18.1 | MIT OR Apache-2.0 | https://github.com/image-rs/image-png | clean |
| poll-promise | 0.3.0 | MIT OR Apache-2.0 | https://github.com/EmbarkStudios/poll-promise | clean |
| polling | 3.11.0 | Apache-2.0 OR MIT | https://github.com/smol-rs/polling | clean |
| pollster | 1.0.2 | MIT OR Apache-2.0 | https://github.com/zesterer/pollster | clean |
| polycool | 0.4.0 | MIT OR Apache-2.0 | https://github.com/linebender/kurbo | clean |
| portable-atomic | 1.15.0 | Apache-2.0 OR MIT | https://github.com/taiki-e/portable-atomic | clean |
| portable-atomic-util | 0.2.8 | Apache-2.0 OR MIT | https://github.com/taiki-e/portable-atomic-util | clean |
| potential_utf | 0.1.6 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| presser | 0.3.1 | MIT OR Apache-2.0 | https://github.com/EmbarkStudios/presser | clean |
| proc-macro-crate | 3.5.0 | MIT OR Apache-2.0 | https://github.com/bkchr/proc-macro-crate | clean |
| proc-macro2 | 1.0.107 | MIT OR Apache-2.0 | https://github.com/dtolnay/proc-macro2 | clean |
| profiling | 1.0.18 | MIT OR Apache-2.0 | https://github.com/aclysma/profiling | clean |
| pxfm | 0.1.30 | BSD-3-Clause OR Apache-2.0 | https://github.com/awxkee/pxfm | clean |
| quick-error | 2.0.1 | MIT/Apache-2.0 | http://github.com/tailhook/quick-error | clean |
| quick-xml | 0.41.0 | MIT | https://github.com/tafia/quick-xml | clean |
| quinn | 0.11.12 | MIT OR Apache-2.0 | https://github.com/quinn-rs/quinn | clean |
| quinn-proto | 0.11.19 | MIT OR Apache-2.0 | https://github.com/quinn-rs/quinn | clean |
| quinn-udp | 0.5.16 | MIT OR Apache-2.0 | https://github.com/quinn-rs/quinn | clean |
| quote | 1.0.47 | MIT OR Apache-2.0 | https://github.com/dtolnay/quote | clean |
| r-efi | 5.3.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later | https://github.com/r-efi/r-efi | noted: OR-license, MIT/Apache-2.0 elected; not compiled for x86_64 Linux (see note) |
| r-efi | 6.0.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later | https://github.com/r-efi/r-efi | noted: OR-license, MIT/Apache-2.0 elected; not compiled for x86_64 Linux (see note) |
| rand | 0.10.4 | MIT OR Apache-2.0 | https://github.com/rust-random/rand | clean |
| rand_core | 0.10.1 | MIT OR Apache-2.0 | https://github.com/rust-random/rand_core | clean |
| rand_pcg | 0.10.2 | MIT OR Apache-2.0 | https://github.com/rust-random/rngs | clean |
| range-alloc | 0.1.5 | MIT OR Apache-2.0 | https://github.com/gfx-rs/range-alloc | clean |
| raw-window-handle | 0.6.2 | MIT OR Apache-2.0 OR Zlib | https://github.com/rust-windowing/raw-window-handle | clean |
| raw-window-metal | 1.1.0 | MIT OR Apache-2.0 | https://github.com/rust-windowing/raw-window-metal | clean |
| read-fonts | 0.41.0 | MIT OR Apache-2.0 | https://github.com/googlefonts/fontations | clean |
| redox_syscall | 0.4.1 | MIT | https://gitlab.redox-os.org/redox-os/syscall | clean |
| redox_syscall | 0.5.18 | MIT | https://gitlab.redox-os.org/redox-os/syscall | clean |
| redox_syscall | 0.9.5 | MIT | https://gitlab.redox-os.org/redox-os/kernel | clean |
| renderdoc-sys | 1.1.0 | MIT OR Apache-2.0 | https://github.com/ebkalderon/renderdoc-rs | clean |
| reqwest | 0.12.28 | MIT OR Apache-2.0 | https://github.com/seanmonstar/reqwest | clean |
| ring | 0.17.14 | Apache-2.0 AND ISC | https://github.com/briansmith/ring | clean |
| roxmltree | 0.21.2 | MIT OR Apache-2.0 | https://github.com/RazrFalcon/roxmltree | clean |
| rusqlite | 0.32.1 | MIT | https://github.com/rusqlite/rusqlite | clean |
| rustc-hash | 1.1.0 | Apache-2.0/MIT | https://github.com/rust-lang-nursery/rustc-hash | clean |
| rustc-hash | 2.1.3 | Apache-2.0 OR MIT | https://github.com/rust-lang/rustc-hash | clean |
| rustc_version | 0.4.1 | MIT OR Apache-2.0 | https://github.com/djc/rustc-version-rs | clean |
| rustix | 0.38.44 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://github.com/bytecodealliance/rustix | clean |
| rustix | 1.1.6 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://github.com/bytecodealliance/rustix | clean |
| rustls | 0.23.46 | Apache-2.0 OR ISC OR MIT | https://github.com/rustls/rustls | clean |
| rustls-pki-types | 1.16.0 | MIT OR Apache-2.0 | https://github.com/rustls/pki-types | clean |
| rustls-webpki | 0.103.17 | ISC | https://github.com/rustls/webpki | clean |
| rustversion | 1.0.23 | MIT OR Apache-2.0 | https://github.com/dtolnay/rustversion | clean |
| ryu | 1.0.23 | Apache-2.0 OR BSL-1.0 | https://github.com/dtolnay/ryu | clean |
| same-file | 1.0.6 | Unlicense/MIT | https://github.com/BurntSushi/same-file | clean |
| scoped-tls | 1.0.1 | MIT/Apache-2.0 | https://github.com/alexcrichton/scoped-tls | clean |
| scopeguard | 1.2.0 | MIT OR Apache-2.0 | https://github.com/bluss/scopeguard | clean |
| sctk-adwaita | 0.10.1 | MIT | https://github.com/PolyMeilex/sctk-adwaita | clean |
| security-framework | 3.7.0 | MIT OR Apache-2.0 | https://github.com/kornelski/rust-security-framework | clean |
| security-framework-sys | 2.17.0 | MIT OR Apache-2.0 | https://github.com/kornelski/rust-security-framework | clean |
| self_cell | 1.3.0 | Apache-2.0 OR GPL-2.0-only | https://github.com/Voultapher/self_cell | noted: OR-license, Apache-2.0 elected (see note) |
| semver | 1.0.28 | MIT OR Apache-2.0 | https://github.com/dtolnay/semver | clean |
| send_wrapper | 0.6.0 | MIT/Apache-2.0 | https://github.com/thk1/send_wrapper | clean |
| serde | 1.0.229 | MIT OR Apache-2.0 | https://github.com/serde-rs/serde | clean |
| serde_core | 1.0.229 | MIT OR Apache-2.0 | https://github.com/serde-rs/serde | clean |
| serde_derive | 1.0.229 | MIT OR Apache-2.0 | https://github.com/serde-rs/serde | clean |
| serde_json | 1.0.154 | MIT OR Apache-2.0 | https://github.com/serde-rs/json | clean |
| serde_repr | 0.1.21 | MIT OR Apache-2.0 | https://github.com/dtolnay/serde-repr | clean |
| serde_spanned | 1.1.3 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml | clean |
| serde_urlencoded | 0.7.1 | MIT/Apache-2.0 | https://github.com/nox/serde_urlencoded | clean |
| shlex | 2.0.1 | MIT OR Apache-2.0 | https://github.com/comex/rust-shlex | clean |
| signal-hook-registry | 1.4.8 | MIT OR Apache-2.0 | https://github.com/vorner/signal-hook | clean |
| simd-adler32 | 0.3.10 | MIT | https://github.com/mcountryman/simd-adler32 | clean |
| simd_cesu8 | 1.2.0 | Apache-2.0 OR MIT | https://github.com/seancroach/simd_cesu8 | clean |
| simdutf8 | 0.1.5 | MIT OR Apache-2.0 | https://github.com/rusticstuff/simdutf8 | clean |
| siphasher | 1.0.4 | MIT OR Apache-2.0 | https://github.com/jedisct1/rust-siphash | clean |
| skrifa | 0.44.0 | MIT OR Apache-2.0 | https://github.com/googlefonts/fontations | clean |
| slab | 0.4.12 | MIT | https://github.com/tokio-rs/slab | clean |
| slotmap | 1.1.1 | Zlib | https://github.com/orlp/slotmap | clean |
| smallvec | 1.16.5 | MIT OR Apache-2.0 | https://github.com/servo/rust-smallvec | clean |
| smithay-client-toolkit | 0.19.2 | MIT | https://github.com/smithay/client-toolkit | clean |
| smithay-client-toolkit | 0.20.0 | MIT | https://github.com/smithay/client-toolkit | clean |
| smithay-clipboard | 0.7.3 | MIT | https://github.com/smithay/smithay-clipboard | clean |
| smol_str | 0.2.2 | MIT OR Apache-2.0 | https://github.com/rust-analyzer/smol_str | clean |
| socket2 | 0.6.5 | MIT OR Apache-2.0 | https://github.com/rust-lang/socket2 | clean |
| spirv | 0.4.0+sdk-1.4.341.0 | Apache-2.0 | https://github.com/gfx-rs/rspirv | clean |
| stable_deref_trait | 1.2.1 | MIT OR Apache-2.0 | https://github.com/storyyeller/stable_deref_trait | clean |
| static_assertions | 1.1.0 | MIT OR Apache-2.0 | https://github.com/nvzqz/static-assertions-rs | clean |
| strict-num | 0.1.1 | MIT | https://github.com/RazrFalcon/strict-num | clean |
| subtle | 2.6.1 | BSD-3-Clause | https://github.com/dalek-cryptography/subtle | clean |
| syn | 2.0.119 | MIT OR Apache-2.0 | https://github.com/dtolnay/syn | clean |
| syn | 3.0.10 | MIT OR Apache-2.0 | https://github.com/dtolnay/syn | clean |
| sync_wrapper | 1.0.2 | Apache-2.0 | https://github.com/Actyx/sync_wrapper | clean |
| synstructure | 0.14.0 | MIT | https://github.com/mystor/synstructure | clean |
| sys-locale | 0.3.2 | MIT OR Apache-2.0 | https://github.com/1Password/sys-locale | clean |
| tempfile | 3.27.0 | MIT OR Apache-2.0 | https://github.com/Stebalien/tempfile | clean |
| termcolor | 1.4.1 | Unlicense OR MIT | https://github.com/BurntSushi/termcolor | clean |
| thiserror | 1.0.69 | MIT OR Apache-2.0 | https://github.com/dtolnay/thiserror | clean |
| thiserror | 2.0.21 | MIT OR Apache-2.0 | https://github.com/dtolnay/thiserror | clean |
| thiserror-impl | 1.0.69 | MIT OR Apache-2.0 | https://github.com/dtolnay/thiserror | clean |
| thiserror-impl | 2.0.21 | MIT OR Apache-2.0 | https://github.com/dtolnay/thiserror | clean |
| tiff | 0.11.3 | MIT | https://github.com/image-rs/image-tiff | clean |
| tiny-skia | 0.11.4 | BSD-3-Clause | https://github.com/RazrFalcon/tiny-skia | clean |
| tiny-skia-path | 0.11.4 | BSD-3-Clause | https://github.com/RazrFalcon/tiny-skia/tree/master/path | clean |
| tinystr | 0.8.5 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| tinyvec | 1.13.3 | Zlib OR Apache-2.0 OR MIT | https://github.com/Lokathor/tinyvec | clean |
| tokio | 1.54.1 | MIT | https://github.com/tokio-rs/tokio | clean |
| tokio-macros | 2.7.2 | MIT | https://github.com/tokio-rs/tokio | clean |
| tokio-rustls | 0.26.6 | MIT OR Apache-2.0 | https://github.com/rustls/tokio-rustls | clean |
| toml | 0.9.12+spec-1.1.0 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml | clean |
| toml_datetime | 0.7.5+spec-1.1.0 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml | clean |
| toml_datetime | 1.1.3+spec-1.1.0 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml | clean |
| toml_edit | 0.25.19+spec-1.1.0 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml | clean |
| toml_parser | 1.1.5+spec-1.1.0 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml | clean |
| toml_writer | 1.2.0+spec-1.1.0 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml | clean |
| tower | 0.5.3 | MIT | https://github.com/tower-rs/tower | clean |
| tower-http | 0.6.11 | MIT | https://github.com/tower-rs/tower-http | clean |
| tower-layer | 0.3.3 | MIT | https://github.com/tower-rs/tower | clean |
| tower-service | 0.3.3 | MIT | https://github.com/tower-rs/tower | clean |
| tracing | 0.1.44 | MIT | https://github.com/tokio-rs/tracing | clean |
| tracing-attributes | 0.1.31 | MIT | https://github.com/tokio-rs/tracing | clean |
| tracing-core | 0.1.36 | MIT | https://github.com/tokio-rs/tracing | clean |
| tree_magic_mini | 3.2.2 | MIT | https://github.com/mbrubeck/tree_magic/ | clean |
| try-lock | 0.2.5 | MIT | https://github.com/seanmonstar/try-lock | clean |
| ttf-parser | 0.25.1 | MIT OR Apache-2.0 | https://github.com/harfbuzz/ttf-parser | clean |
| type-map | 0.5.1 | MIT/Apache-2.0 | https://github.com/kardeiz/type-map | clean |
| uds_windows | 1.2.1 | MIT | https://github.com/haraldh/rust_uds_windows | clean |
| unicode-general-category | 1.1.0 | Apache-2.0 | https://github.com/yeslogic/unicode-general-category | clean |
| unicode-ident | 1.0.27 | (MIT OR Apache-2.0) AND Unicode-3.0 | https://github.com/dtolnay/unicode-ident | clean |
| unicode-properties | 0.1.5 | (MIT OR Apache-2.0) AND Unicode-3.0 | https://github.com/unicode-rs/unicode-properties | clean |
| unicode-script | 0.5.9 | MIT OR Apache-2.0 | https://github.com/unicode-rs/unicode-script | clean |
| unicode-segmentation | 1.13.3 | MIT OR Apache-2.0 | https://github.com/unicode-rs/unicode-segmentation | clean |
| unicode-width | 0.2.3 | MIT OR Apache-2.0 | https://github.com/unicode-rs/unicode-width | clean |
| untrusted | 0.9.0 | ISC | https://github.com/briansmith/untrusted | clean |
| url | 2.5.8 | MIT OR Apache-2.0 | https://github.com/servo/rust-url | clean |
| utf8_iter | 1.1.0 | Apache-2.0 OR MIT | https://github.com/hsivonen/utf8_iter | clean |
| utf_types | 1.0.0 | Apache-2.0 OR MIT | https://github.com/hsivonen/utf_types | clean |
| uuid | 1.29.3 | Apache-2.0 OR MIT | https://github.com/uuid-rs/uuid | clean |
| vcpkg | 0.2.15 | MIT/Apache-2.0 | https://github.com/mcgoo/vcpkg-rs | clean |
| vello_common | 0.2.0 | Apache-2.0 OR MIT | https://github.com/linebender/vello | clean |
| vello_cpu | 0.2.0 | Apache-2.0 OR MIT | https://github.com/linebender/vello | clean |
| version_check | 0.9.5 | MIT/Apache-2.0 | https://github.com/SergioBenitez/version_check | clean |
| walkdir | 2.5.0 | Unlicense/MIT | https://github.com/BurntSushi/walkdir | clean |
| want | 0.3.2 | MIT | https://github.com/seanmonstar/want | clean |
| wasi | 0.11.1+wasi-snapshot-preview1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://github.com/bytecodealliance/wasi | clean |
| wasip2 | 1.0.4+wasi-0.2.12 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://github.com/bytecodealliance/wasi-rs | clean |
| wasm-bindgen | 0.2.130 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen | clean |
| wasm-bindgen-futures | 0.4.80 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/futures | clean |
| wasm-bindgen-macro | 0.2.130 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/macro | clean |
| wasm-bindgen-macro-support | 0.2.130 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen/tree/main/crates/macro-support | clean |
| wasm-bindgen-shared | 0.2.130 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/shared | clean |
| wayland-backend | 0.3.17 | MIT | https://github.com/smithay/wayland-rs | clean |
| wayland-client | 0.31.15 | MIT | https://github.com/smithay/wayland-rs | clean |
| wayland-csd-frame | 0.3.0 | MIT | https://github.com/rust-windowing/wayland-csd-frame | clean |
| wayland-cursor | 0.31.14 | MIT | https://github.com/smithay/wayland-rs | clean |
| wayland-protocols | 0.32.13 | MIT | https://github.com/smithay/wayland-rs | clean |
| wayland-protocols-experimental | 20250721.0.1 | MIT | https://github.com/smithay/wayland-rs | clean |
| wayland-protocols-misc | 0.3.12 | MIT | https://github.com/smithay/wayland-rs | clean |
| wayland-protocols-plasma | 0.3.12 | MIT | https://github.com/smithay/wayland-rs | clean |
| wayland-protocols-wlr | 0.3.12 | MIT | https://github.com/smithay/wayland-rs | clean |
| wayland-scanner | 0.31.11 | MIT | https://github.com/smithay/wayland-rs | clean |
| wayland-sys | 0.31.11 | MIT | https://github.com/smithay/wayland-rs | clean |
| web-sys | 0.3.107 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/web-sys | clean |
| web-time | 1.1.0 | MIT OR Apache-2.0 | https://github.com/daxpedda/web-time | clean |
| webbrowser | 1.2.7 | MIT OR Apache-2.0 | https://github.com/amodm/webbrowser-rs | clean |
| webpki-roots | 1.0.12 | CDLA-Permissive-2.0 | https://github.com/rustls/webpki-roots | noted: CDLA-Permissive-2.0 permissive data license (see note) |
| weezl | 0.1.12 | MIT OR Apache-2.0 | https://github.com/image-rs/weezl | clean |
| wgpu | 30.0.2 | MIT OR Apache-2.0 | https://github.com/gfx-rs/wgpu | clean |
| wgpu-core | 30.0.2 | MIT OR Apache-2.0 | https://github.com/gfx-rs/wgpu | clean |
| wgpu-core-deps-apple | 30.0.2 | MIT OR Apache-2.0 | https://github.com/gfx-rs/wgpu | clean |
| wgpu-core-deps-emscripten | 30.0.2 | MIT OR Apache-2.0 | https://github.com/gfx-rs/wgpu | clean |
| wgpu-core-deps-wasm | 30.0.2 | MIT OR Apache-2.0 | https://github.com/gfx-rs/wgpu | clean |
| wgpu-core-deps-windows-linux-android | 30.0.2 | MIT OR Apache-2.0 | https://github.com/gfx-rs/wgpu | clean |
| wgpu-hal | 30.0.2 | MIT OR Apache-2.0 | https://github.com/gfx-rs/wgpu | clean |
| wgpu-naga-bridge | 30.0.2 | MIT OR Apache-2.0 | https://github.com/gfx-rs/wgpu | clean |
| wgpu-types | 30.0.2 | MIT OR Apache-2.0 | https://github.com/gfx-rs/wgpu | clean |
| winapi-util | 0.1.11 | Unlicense OR MIT | https://github.com/BurntSushi/winapi-util | clean |
| windows | 0.62.2 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows-collections | 0.3.2 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows-core | 0.62.2 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows-future | 0.3.2 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows-implement | 0.60.2 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows-interface | 0.59.3 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows-link | 0.2.1 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows-numerics | 0.3.1 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows-result | 0.4.1 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows-strings | 0.5.1 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows-sys | 0.52.0 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows-sys | 0.61.2 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows-targets | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows-threading | 0.2.1 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows_aarch64_gnullvm | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows_aarch64_msvc | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows_i686_gnu | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows_i686_gnullvm | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows_i686_msvc | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows_x86_64_gnu | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows_x86_64_gnullvm | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows_x86_64_msvc | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| winit | 0.30.13 | Apache-2.0 | https://github.com/rust-windowing/winit | clean |
| winnow | 0.7.15 | MIT | https://github.com/winnow-rs/winnow | clean |
| winnow | 1.0.5 | MIT | https://github.com/winnow-rs/winnow | clean |
| wit-bindgen | 0.57.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://github.com/bytecodealliance/wit-bindgen | clean |
| wl-clipboard-rs | 0.9.5 | MIT OR Apache-2.0 | https://github.com/YaLTeR/wl-clipboard-rs | clean |
| writeable | 0.6.4 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| x11-dl | 2.21.0 | MIT | https://github.com/AltF02/x11-rs.git | clean |
| x11rb | 0.13.2 | MIT OR Apache-2.0 | https://github.com/psychon/x11rb | clean |
| x11rb-protocol | 0.13.2 | MIT OR Apache-2.0 | https://github.com/psychon/x11rb | clean |
| xcursor | 0.3.11 | MIT | https://github.com/esposm03/xcursor-rs | clean |
| xkbcommon-dl | 0.4.2 | MIT | https://github.com/rust-windowing/xkbcommon-dl | clean |
| xkeysym | 0.2.1 | MIT OR Apache-2.0 OR Zlib | https://github.com/notgull/xkeysym | clean |
| xml-rs | 0.8.29 | MIT | https://github.com/kornelski/xml-rs | clean |
| yeslogic-fontconfig-sys | 6.0.1 | MIT | https://github.com/yeslogic/fontconfig-rs | clean |
| yoke | 0.8.4 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| yoke-derive | 0.8.4 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| zbus | 5.19.0 | MIT | https://github.com/z-galaxy/zbus/ | clean |
| zbus-lockstep | 0.5.2 | MIT | https://github.com/luukvanderduim/zbus-lockstep | clean |
| zbus-lockstep-macros | 0.5.2 | MIT | https://github.com/luukvanderduim/zbus-lockstep | clean |
| zbus_macros | 5.19.0 | MIT | https://github.com/z-galaxy/zbus/ | clean |
| zbus_names | 4.3.5 | MIT | https://github.com/z-galaxy/zbus/ | clean |
| zbus_xml | 5.2.1 | MIT | https://github.com/z-galaxy/zbus/ | clean |
| zcheapstr | 1.2.0 | MIT | https://github.com/z-galaxy/zcheapstr/ | clean |
| zerocopy | 0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT | https://github.com/google/zerocopy | clean |
| zerocopy-derive | 0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT | https://github.com/google/zerocopy | clean |
| zerofrom | 0.1.8 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| zerofrom-derive | 0.1.8 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| zeroize | 1.9.1 | Apache-2.0 OR MIT | https://github.com/RustCrypto/utils | clean |
| zerotrie | 0.2.5 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| zerovec | 0.11.9 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| zerovec-derive | 0.11.7 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| zlib-rs | 0.6.8 | Zlib | https://github.com/trifectatechfoundation/zlib-rs | clean |
| zmij | 1.0.23 | MIT | https://github.com/dtolnay/zmij | clean |
| zune-core | 0.5.3 | MIT OR Apache-2.0 OR Zlib | https://github.com/etemesi254/zune-image | clean |
| zune-jpeg | 0.5.15 | MIT OR Apache-2.0 OR Zlib | https://github.com/etemesi254/zune-image/tree/dev/crates/zune-jpeg | clean |
| zvariant | 5.15.1 | MIT | https://github.com/z-galaxy/zbus/ | clean |
| zvariant_derive | 5.15.1 | MIT | https://github.com/z-galaxy/zbus/ | clean |
| zvariant_utils | 4.2.0 | MIT | https://github.com/z-galaxy/zbus/ | clean |

## 4. Community Data License Agreement - Permissive - Version 2.0 (webpki-roots)

> Full text of the CDLA-Permissive-2.0 agreement, reproduced
> verbatim from the `LICENSE` file of the `webpki-roots` 1.0.12
> crate (registry source, sha256 of that file:
> `e271993808fec50ab29350b39539cdec611a9103f827e0aa26d61da70e2d33f8`).
> Required by CDLA-Permissive-2.0 §2.1: the agreement text must be
> made available with the shared Data (the Mozilla root-certificate
> list bundled by `webpki-roots`).

# Community Data License Agreement - Permissive - Version 2.0

This is the Community Data License Agreement - Permissive, Version
2.0 (the "agreement"). Data Provider(s) and Data Recipient(s) agree
as follows:

## 1. Provision of the Data

1.1. A Data Recipient may use, modify, and share the Data made
available by Data Provider(s) under this agreement if that Data
Recipient follows the terms of this agreement.

1.2. This agreement does not impose any restriction on a Data
Recipient's use, modification, or sharing of any portions of the
Data that are in the public domain or that may be used, modified,
or shared under any other legal exception or limitation.

## 2. Conditions for Sharing Data

2.1. A Data Recipient may share Data, with or without modifications, so
long as the Data Recipient makes available the text of this agreement
with the shared Data.

## 3. No Restrictions on Results

3.1. This agreement does not impose any restriction or obligations
with respect to the use, modification, or sharing of Results.

## 4. No Warranty; Limitation of Liability

4.1. All Data Recipients receive the Data subject to the following
terms:

THE DATA IS PROVIDED ON AN "AS IS" BASIS, WITHOUT REPRESENTATIONS,
WARRANTIES OR CONDITIONS OF ANY KIND, EITHER EXPRESS OR IMPLIED
INCLUDING, WITHOUT LIMITATION, ANY WARRANTIES OR CONDITIONS OF TITLE,
NON-INFRINGEMENT, MERCHANTABILITY OR FITNESS FOR A PARTICULAR PURPOSE.

NO DATA PROVIDER SHALL HAVE ANY LIABILITY FOR ANY DIRECT, INDIRECT,
INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING
WITHOUT LIMITATION LOST PROFITS), HOWEVER CAUSED AND ON ANY THEORY OF
LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE DATA OR RESULTS,
EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGES.

## 5. Definitions

5.1. "Data" means the material received by a Data Recipient under
this agreement.

5.2. "Data Provider" means any person who is the source of Data
provided under this agreement and in reliance on a Data Recipient's
agreement to its terms.

5.3. "Data Recipient" means any person who receives Data directly
or indirectly from a Data Provider and agrees to the terms of this
agreement.

5.4. "Results" means any outcome obtained by computational analysis
of Data, including for example machine learning models and models'
insights.
