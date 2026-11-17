# Mimint Phase 2 — License Audit

> SOURCE OF TRUTH: this file is the authoritative license audit for the
> Phase 2 dependency tree. `phase1/LICENSE-AUDIT.md` covers the Phase 1
> tree only and is superseded by this file (see the notice at its top).

## Fix-pass addendum (2026-11-17)

The Phase 2 review fix pass (findings F2-01..F2-22) re-ran this audit
from scratch: `cargo metadata --format-version 1` on vega against the
updated `Cargo.lock` (Rust 1.98.1), and the regenerated table was
compared byte-for-byte with the table below. Result: IDENTICAL — 156
packages, same names, versions, and declared licenses; 0 new
packages, 0 forbidden, 0 version changes.

- The fix pass adds exactly two new DIRECT dependencies, both already
  present in the Phase 2 resolve as transitive dependencies, so
  `Cargo.lock` grows by 2 lines only (dependency edges, no new
  packages): `zeroize` 1.9.1 (`Apache-2.0 OR MIT`; secret wiping, with
  the `alloc` feature for the `String` impl) and `libc` 0.2.191 (`MIT
  OR Apache-2.0`; `O_NOFOLLOW`/`ELOOP` for the file-vault descriptor
  check). Both were already counted clean in the table below; no new
  verdict was needed.
- The `r-efi` election stands: re-verified 2026-11-17, 0 occurrences
  in `cargo tree` for x86_64-unknown-linux-gnu and `--target all`, 0
  compiled artifacts under `target/debug/deps/`.
- The `webpki-roots` CDLA-Permissive-2.0 note stands unchanged.
- Repo license still TBD (a user decision is required before Phase
  8); no license file added.

Source: `cargo metadata --format-version 1` run on vega against the
workspace `Cargo.lock` (Rust 1.98.1), 2026-11-17. Every package in the
resolve is listed below with its declared `license` field exactly as
reported by cargo metadata, plus its repository URL and a verdict.

Packages in resolve: 156 (152 clean third-party, 2 workspace own-code, 2 noted with explanations below, 0 need review, 0 forbidden).

Verdict rules: clean = declared license consists only of MIT /
Apache-2.0 / BSD (1/2/3-Clause) / ISC / Zlib / Unlicense / Unicode-3.0 /
Unicode-DFS-2016 / OpenSSL / CC0-1.0 / BSL-1.0 (SPDX OR/AND combos
counted clean when every alternative is in that set; the LLVM-exception
is an additional permission on Apache-2.0, not a restriction). Any
GPL / AGPL / LGPL term that actually applies would be FORBIDDEN and
stop the build; none applies — see the r-efi note for the one
LGPL token in the resolve, which sits behind an OR and is not
compiled for this target.

## Notes

- Phase 2 adds exactly one new direct dependency: `toml` 0.9 (TOML
  config parsing), which pulls 9 transitive crates (`equivalent`,
  `hashbrown` 0.17.1, `indexmap`, `serde_spanned`, `toml_datetime`,
  `toml_parser`, `toml_writer`, `winnow` 0.7.15, `winnow` 1.0.5).
  All 10 are MIT / Apache-2.0. No other direct dependency was added:
  no CLI parser, no async-trait, no keyring, no `dirs` crate (config
  paths are resolved by hand; the provider trait uses boxed futures).
- `rusqlite` is MIT; with the `bundled` feature it builds SQLite from
  source, and SQLite itself is public domain (see `libsqlite3-sys`, MIT).
- `ring` declares `Apache-2.0 AND ISC` (both permissive, both apply).
  This is the declared license string from cargo metadata, recorded
  exactly; no other license is claimed for it here.
- `r-efi` 6.0.0 declares `MIT OR Apache-2.0 OR LGPL-2.1-or-later`. The
  OR means any one of the three may be elected; this project elects
  MIT / Apache-2.0, so no LGPL term applies. Verified 2026-11-17:
  `r-efi` appears 0 times in `cargo tree` for x86_64-unknown-linux-gnu
  and 0 times with `--target all`, and has no compiled artifact under
  `target/debug/deps/`. It is present in Cargo.lock / the resolve only.
  No dependency was swapped or removed over this; the election stands
  from Phase 1.
- `webpki-roots` declares `CDLA-Permissive-2.0`, the permissive data
  license of the Mozilla root-certificate list it bundles. It is a
  permissive (non-copyleft) license with notice obligations, and it is
  the standard license for the CA roots every rustls user ships. It
  IS compiled into the Linux binaries (via `rustls-tls`), unlike
  `r-efi`. Flagged here for coordinator awareness, not hidden.
- `memchr` declares `Unlicense OR MIT`: Unlicense is a public-domain
  dedication and MIT is available as the alternative; counted clean.
- Workspace members `mimint-core` and `mimint-cli` are own clean-room
  code; the repository license is still TBD and no license file was added.
- All other entries are unchanged from Phase 1 (same name, version,
  and declared license); the 10 new entries are the `toml` subtree above.

## Full table

| crate | version | license (declared) | repository | verdict |
|---|---|---|---|---|
| ahash | 0.8.12 | MIT OR Apache-2.0 | https://github.com/tkaitchuck/ahash | clean |
| atomic-waker | 1.1.2 | Apache-2.0 OR MIT | https://github.com/smol-rs/atomic-waker | clean |
| base64 | 0.22.1 | MIT OR Apache-2.0 | https://github.com/marshallpierce/rust-base64 | clean |
| base64 | 0.23.1 | MIT OR Apache-2.0 | https://github.com/marshallpierce/rust-base64 | clean |
| bitflags | 2.13.2 | MIT OR Apache-2.0 | https://github.com/bitflags/bitflags | clean |
| bumpalo | 3.20.4 | MIT OR Apache-2.0 | https://github.com/fitzgen/bumpalo | clean |
| bytes | 1.12.1 | MIT | https://github.com/tokio-rs/bytes | clean |
| cc | 1.11.2 | MIT OR Apache-2.0 | https://github.com/rust-lang/cc-rs | clean |
| cfg-if | 1.0.5 | MIT OR Apache-2.0 | https://github.com/rust-lang/cfg-if | clean |
| cfg_aliases | 0.2.2 | MIT | https://github.com/katharostech/cfg_aliases | clean |
| chacha20 | 0.10.2 | MIT OR Apache-2.0 | https://github.com/RustCrypto/stream-ciphers | clean |
| cpufeatures | 0.3.1 | MIT OR Apache-2.0 | https://github.com/RustCrypto/utils | clean |
| displaydoc | 0.2.7 | MIT OR Apache-2.0 | https://github.com/yaahc/displaydoc | clean |
| equivalent | 1.0.3 | Apache-2.0 OR MIT | https://github.com/indexmap-rs/equivalent | clean |
| fallible-iterator | 0.3.0 | MIT/Apache-2.0 | https://github.com/sfackler/rust-fallible-iterator | clean |
| fallible-streaming-iterator | 0.1.9 | MIT/Apache-2.0 | https://github.com/sfackler/fallible-streaming-iterator | clean |
| find-msvc-tools | 0.1.15 | MIT OR Apache-2.0 | https://github.com/rust-lang/cc-rs | clean |
| form_urlencoded | 1.2.2 | MIT OR Apache-2.0 | https://github.com/servo/rust-url | clean |
| futures-channel | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs | clean |
| futures-core | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs | clean |
| futures-task | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs | clean |
| futures-util | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/futures-rs | clean |
| getrandom | 0.2.17 | MIT OR Apache-2.0 | https://github.com/rust-random/getrandom | clean |
| getrandom | 0.4.3 | MIT OR Apache-2.0 | https://github.com/rust-random/getrandom | clean |
| hashbrown | 0.14.5 | MIT OR Apache-2.0 | https://github.com/rust-lang/hashbrown | clean |
| hashbrown | 0.17.1 | MIT OR Apache-2.0 | https://github.com/rust-lang/hashbrown | clean |
| hashlink | 0.9.1 | MIT OR Apache-2.0 | https://github.com/kyren/hashlink | clean |
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
| indexmap | 2.14.2 | Apache-2.0 OR MIT | https://github.com/indexmap-rs/indexmap | clean |
| ipnet | 2.12.2 | MIT OR Apache-2.0 | https://github.com/krisprice/ipnet | clean |
| itoa | 1.0.19 | MIT OR Apache-2.0 | https://github.com/dtolnay/itoa | clean |
| js-sys | 0.3.107 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/js-sys | clean |
| libc | 0.2.191 | MIT OR Apache-2.0 | https://github.com/rust-lang/libc | clean |
| libsqlite3-sys | 0.30.1 | MIT | https://github.com/rusqlite/rusqlite | clean |
| litemap | 0.8.3 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| log | 0.4.35 | MIT OR Apache-2.0 | https://github.com/rust-lang/log | clean |
| lru-slab | 0.1.3 | MIT OR Apache-2.0 OR Zlib | https://github.com/Ralith/lru-slab | clean |
| memchr | 2.8.3 | Unlicense OR MIT | https://github.com/BurntSushi/memchr | clean |
| mimint-cli | 0.1.0 | (none) | — | own code (repo license TBD, no license file added) |
| mimint-core | 0.1.0 | (none) | — | own code (repo license TBD, no license file added) |
| mio | 1.2.5 | MIT | https://github.com/tokio-rs/mio | clean |
| once_cell | 1.21.4 | MIT OR Apache-2.0 | https://github.com/matklad/once_cell | clean |
| percent-encoding | 2.3.2 | MIT OR Apache-2.0 | https://github.com/servo/rust-url/ | clean |
| pin-project-lite | 0.2.17 | Apache-2.0 OR MIT | https://github.com/taiki-e/pin-project-lite | clean |
| pkg-config | 0.3.34 | MIT OR Apache-2.0 | https://github.com/rust-lang/pkg-config-rs | clean |
| potential_utf | 0.1.6 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| proc-macro2 | 1.0.107 | MIT OR Apache-2.0 | https://github.com/dtolnay/proc-macro2 | clean |
| quinn | 0.11.12 | MIT OR Apache-2.0 | https://github.com/quinn-rs/quinn | clean |
| quinn-proto | 0.11.19 | MIT OR Apache-2.0 | https://github.com/quinn-rs/quinn | clean |
| quinn-udp | 0.5.16 | MIT OR Apache-2.0 | https://github.com/quinn-rs/quinn | clean |
| quote | 1.0.47 | MIT OR Apache-2.0 | https://github.com/dtolnay/quote | clean |
| r-efi | 6.0.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later | https://github.com/r-efi/r-efi | noted: OR-license, MIT/Apache-2.0 elected; not compiled for x86_64 Linux (see note) |
| rand | 0.10.4 | MIT OR Apache-2.0 | https://github.com/rust-random/rand | clean |
| rand_core | 0.10.1 | MIT OR Apache-2.0 | https://github.com/rust-random/rand_core | clean |
| rand_pcg | 0.10.2 | MIT OR Apache-2.0 | https://github.com/rust-random/rngs | clean |
| reqwest | 0.12.28 | MIT OR Apache-2.0 | https://github.com/seanmonstar/reqwest | clean |
| ring | 0.17.14 | Apache-2.0 AND ISC | https://github.com/briansmith/ring | clean |
| rusqlite | 0.32.1 | MIT | https://github.com/rusqlite/rusqlite | clean |
| rustc-hash | 2.1.3 | Apache-2.0 OR MIT | https://github.com/rust-lang/rustc-hash | clean |
| rustls | 0.23.46 | Apache-2.0 OR ISC OR MIT | https://github.com/rustls/rustls | clean |
| rustls-pki-types | 1.16.0 | MIT OR Apache-2.0 | https://github.com/rustls/pki-types | clean |
| rustls-webpki | 0.103.17 | ISC | https://github.com/rustls/webpki | clean |
| rustversion | 1.0.23 | MIT OR Apache-2.0 | https://github.com/dtolnay/rustversion | clean |
| ryu | 1.0.23 | Apache-2.0 OR BSL-1.0 | https://github.com/dtolnay/ryu | clean |
| serde | 1.0.229 | MIT OR Apache-2.0 | https://github.com/serde-rs/serde | clean |
| serde_core | 1.0.229 | MIT OR Apache-2.0 | https://github.com/serde-rs/serde | clean |
| serde_derive | 1.0.229 | MIT OR Apache-2.0 | https://github.com/serde-rs/serde | clean |
| serde_json | 1.0.154 | MIT OR Apache-2.0 | https://github.com/serde-rs/json | clean |
| serde_spanned | 1.1.3 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml | clean |
| serde_urlencoded | 0.7.1 | MIT/Apache-2.0 | https://github.com/nox/serde_urlencoded | clean |
| shlex | 2.0.1 | MIT OR Apache-2.0 | https://github.com/comex/rust-shlex | clean |
| slab | 0.4.12 | MIT | https://github.com/tokio-rs/slab | clean |
| smallvec | 1.16.5 | MIT OR Apache-2.0 | https://github.com/servo/rust-smallvec | clean |
| socket2 | 0.6.5 | MIT OR Apache-2.0 | https://github.com/rust-lang/socket2 | clean |
| stable_deref_trait | 1.2.1 | MIT OR Apache-2.0 | https://github.com/storyyeller/stable_deref_trait | clean |
| subtle | 2.6.1 | BSD-3-Clause | https://github.com/dalek-cryptography/subtle | clean |
| syn | 2.0.119 | MIT OR Apache-2.0 | https://github.com/dtolnay/syn | clean |
| syn | 3.0.10 | MIT OR Apache-2.0 | https://github.com/dtolnay/syn | clean |
| sync_wrapper | 1.0.2 | Apache-2.0 | https://github.com/Actyx/sync_wrapper | clean |
| synstructure | 0.14.0 | MIT | https://github.com/mystor/synstructure | clean |
| thiserror | 2.0.21 | MIT OR Apache-2.0 | https://github.com/dtolnay/thiserror | clean |
| thiserror-impl | 2.0.21 | MIT OR Apache-2.0 | https://github.com/dtolnay/thiserror | clean |
| tinystr | 0.8.5 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| tinyvec | 1.13.3 | Zlib OR Apache-2.0 OR MIT | https://github.com/Lokathor/tinyvec | clean |
| tokio | 1.54.1 | MIT | https://github.com/tokio-rs/tokio | clean |
| tokio-macros | 2.7.2 | MIT | https://github.com/tokio-rs/tokio | clean |
| tokio-rustls | 0.26.6 | MIT OR Apache-2.0 | https://github.com/rustls/tokio-rustls | clean |
| toml | 0.9.12+spec-1.1.0 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml | clean |
| toml_datetime | 0.7.5+spec-1.1.0 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml | clean |
| toml_parser | 1.1.5+spec-1.1.0 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml | clean |
| toml_writer | 1.2.0+spec-1.1.0 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml | clean |
| tower | 0.5.3 | MIT | https://github.com/tower-rs/tower | clean |
| tower-http | 0.6.11 | MIT | https://github.com/tower-rs/tower-http | clean |
| tower-layer | 0.3.3 | MIT | https://github.com/tower-rs/tower | clean |
| tower-service | 0.3.3 | MIT | https://github.com/tower-rs/tower | clean |
| tracing | 0.1.44 | MIT | https://github.com/tokio-rs/tracing | clean |
| tracing-core | 0.1.36 | MIT | https://github.com/tokio-rs/tracing | clean |
| try-lock | 0.2.5 | MIT | https://github.com/seanmonstar/try-lock | clean |
| unicode-ident | 1.0.27 | (MIT OR Apache-2.0) AND Unicode-3.0 | https://github.com/dtolnay/unicode-ident | clean |
| untrusted | 0.9.0 | ISC | https://github.com/briansmith/untrusted | clean |
| url | 2.5.8 | MIT OR Apache-2.0 | https://github.com/servo/rust-url | clean |
| utf8_iter | 1.1.0 | Apache-2.0 OR MIT | https://github.com/hsivonen/utf8_iter | clean |
| utf_types | 1.0.0 | Apache-2.0 OR MIT | https://github.com/hsivonen/utf_types | clean |
| vcpkg | 0.2.15 | MIT/Apache-2.0 | https://github.com/mcgoo/vcpkg-rs | clean |
| version_check | 0.9.5 | MIT/Apache-2.0 | https://github.com/SergioBenitez/version_check | clean |
| want | 0.3.2 | MIT | https://github.com/seanmonstar/want | clean |
| wasi | 0.11.1+wasi-snapshot-preview1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | https://github.com/bytecodealliance/wasi | clean |
| wasm-bindgen | 0.2.130 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen | clean |
| wasm-bindgen-futures | 0.4.80 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/futures | clean |
| wasm-bindgen-macro | 0.2.130 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/macro | clean |
| wasm-bindgen-macro-support | 0.2.130 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen/tree/main/crates/macro-support | clean |
| wasm-bindgen-shared | 0.2.130 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/shared | clean |
| web-sys | 0.3.107 | MIT OR Apache-2.0 | https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/web-sys | clean |
| web-time | 1.1.0 | MIT OR Apache-2.0 | https://github.com/daxpedda/web-time | clean |
| webpki-roots | 1.0.12 | CDLA-Permissive-2.0 | https://github.com/rustls/webpki-roots | noted: CDLA-Permissive-2.0 permissive data license (see note) |
| windows-link | 0.2.1 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows-sys | 0.52.0 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows-sys | 0.61.2 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows-targets | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows_aarch64_gnullvm | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows_aarch64_msvc | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows_i686_gnu | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows_i686_gnullvm | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows_i686_msvc | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows_x86_64_gnu | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows_x86_64_gnullvm | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| windows_x86_64_msvc | 0.52.6 | MIT OR Apache-2.0 | https://github.com/microsoft/windows-rs | clean |
| winnow | 0.7.15 | MIT | https://github.com/winnow-rs/winnow | clean |
| winnow | 1.0.5 | MIT | https://github.com/winnow-rs/winnow | clean |
| writeable | 0.6.4 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| yoke | 0.8.4 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| yoke-derive | 0.8.4 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| zerocopy | 0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT | https://github.com/google/zerocopy | clean |
| zerocopy-derive | 0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT | https://github.com/google/zerocopy | clean |
| zerofrom | 0.1.8 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| zerofrom-derive | 0.1.8 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| zeroize | 1.9.1 | Apache-2.0 OR MIT | https://github.com/RustCrypto/utils | clean |
| zerotrie | 0.2.5 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| zerovec | 0.11.9 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| zerovec-derive | 0.11.7 | Unicode-3.0 | https://github.com/unicode-org/icu4x | clean |
| zmij | 1.0.23 | MIT | https://github.com/dtolnay/zmij | clean |
