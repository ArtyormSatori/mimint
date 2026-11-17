# Mimint Phase 3 — License Audit

> SUPERSEDED: this file covers the Phase 3 dependency tree only (494
> packages). The authoritative license audit for the current tree is
> `phase4/LICENSE-AUDIT.md` (495 packages, Phase 4). This file is kept
> as a historical record; do not treat it as the source of truth.

> SOURCE OF TRUTH (historical): this file was the authoritative license audit for the
> Phase 3 dependency tree. `phase2/LICENSE-AUDIT.md` covers the Phase 2
> tree only and is superseded by this file (see the notice at its top).

Source: `cargo metadata --format-version 1` run on vega against the
workspace `Cargo.lock` (Rust 1.98.1), 2026-11-17. Every package in the
resolve is listed below with its declared `license` field exactly as
reported by cargo metadata, plus its repository URL and a verdict.

Packages in resolve: 494 (485 clean third-party, 4 workspace own-code, 5 noted with explanations below, 0 need review, 0 forbidden).

Verdict rules: clean = declared license consists only of MIT /
Apache-2.0 / BSD (1/2/3-Clause) / ISC / Zlib / Unlicense / Unicode-3.0 /
Unicode-DFS-2016 / OpenSSL / CC0-1.0 / BSL-1.0 (SPDX OR/AND combos
counted clean when every alternative is in that set; the LLVM-exception
is an additional permission on Apache-2.0, not a restriction). Any
GPL / AGPL / LGPL term that actually applies would be FORBIDDEN and
stop the build; none applies — the only GPL/LGPL tokens in the resolve
sit behind ORs and are elected away (see the self_cell and r-efi notes).

## Notes

- Phase 3 adds these new DIRECT dependencies, all MIT OR Apache-2.0:
  `x11rb` 0.13.2 (X11 reference body), `core-graphics` 0.25.0 +
  `core-foundation` 0.10.1 (macOS body, cfg-gated), `security-framework`
  3.7.0 (macOS Keychain vault, cfg-gated), `eframe`/`egui` =0.37.0
  (desktop UI, pinned to the Phase 0 measured versions). The egui tree
  accounts for the growth from 156 to 494 packages in the resolve.
  No other direct dependency was added: the body contract, the
  conformance harness, and the UI worker use only the existing
  workspace stack (tokio, serde_json, rusqlite bundled).
- `self_cell` 1.3.0 declares `Apache-2.0 OR GPL-2.0-only`. The OR
  means either may be elected; this project elects Apache-2.0, so no
  GPL term applies. Unlike `r-efi`, `self_cell` IS compiled into the
  Linux binaries (via `epaint` <- `egui` <- `eframe` <- `mimint-ui`,
  verified with `cargo tree --target x86_64-unknown-linux-gnu -i
  self_cell` 2026-11-17). The election is recorded here explicitly
  rather than hidden.
- `r-efi` 5.3.0 and 6.0.0 declare `MIT OR Apache-2.0 OR
  LGPL-2.1-or-later`. The OR means any one of the three may be
  elected; this project elects MIT / Apache-2.0, so no LGPL term
  applies. Re-verified 2026-11-17: `r-efi` appears 0 times in `cargo
  tree` for x86_64-unknown-linux-gnu. Both versions are present in
  Cargo.lock / the resolve only (two versions because the egui tree
  resolves an older one alongside the Phase 1/2 one).
- `epaint_default_fonts` 0.37.0 declares `(MIT OR Apache-2.0) AND
  OFL-1.1 AND Ubuntu-font-1.0`: the code is MIT/Apache-2.0, and the
  bundled font data is under the SIL Open Font License 1.1 and the
  Ubuntu Font License 1.0. Both are permissive font licenses that
  allow redistribution with their notices kept. The font data IS
  compiled into the Linux binaries (via `epaint`). Consequence:
  binary distributions of the desktop app must keep the font license
  notices; this is a packaging obligation for Phase 8, recorded here
  so it is not discovered late. Not forbidden.
- `webpki-roots` declares `CDLA-Permissive-2.0`, the permissive data
  license of the Mozilla root-certificate list it bundles. It IS
  compiled into the Linux binaries (via `rustls-tls`). Carried
  unchanged from the Phase 1/2 notes.
- 18 crates declare `Unicode-3.0` (Unicode data files, e.g. character
  tables used by text layout). The Unicode License Agreement for data
  files is a permissive license with a notice obligation; counted
  clean under the verdict rules above.
- Workspace members `mimint-core`, `mimint-cli`, `mimint-body`, and
  `mimint-ui` are own clean-room code; the repository license is
  still TBD (a user decision is required before Phase 8) and no
  license file was added.
- The macOS-only dependencies (`core-graphics`, `core-foundation`,
  `security-framework`) appear in the resolve for all targets but are
  compiled only for Apple targets; `cargo tree --target
  x86_64-apple-darwin` confirms they attach under `mimint-core` /
  `mimint-body` on that target. Their licenses are MIT OR Apache-2.0.

## Table

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
| mimint-body | 0.1.0 | (none) | — | own code (repo license TBD, no license file added) |
| mimint-cli | 0.1.0 | (none) | — | own code (repo license TBD, no license file added) |
| mimint-core | 0.1.0 | (none) | — | own code (repo license TBD, no license file added) |
| mimint-ui | 0.1.0 | (none) | — | own code (repo license TBD, no license file added) |
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
