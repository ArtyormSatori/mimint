#!/bin/bash
# package-common.sh — shared helpers for the Mimint packaging scripts (P8.4).
# Sourced by package-linux.sh / package-macos.sh / package-windows.sh /
# package-android.sh. Not meant to be run directly.
#
# License rule (P8.1 L-2): every distributable artifact MUST ship
# LICENSE + NOTICE + docs/character-license.md. THIRD-PARTY-NOTICES.md
# ships alongside them. Character image/video/look assets MUST NOT be
# in any Apache 2.0 artifact (G3 negative check).

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="$REPO_ROOT/dist"

# License files that MUST be present in every artifact (relative to repo root).
LICENSE_FILES=(
    "LICENSE"
    "NOTICE"
    "docs/character-license.md"
    "THIRD-PARTY-NOTICES.md"
)

# stage_license_files <dest-dir>
# Copies the four license files into <dest-dir>, preserving the
# docs/character-license.md relative path, and verifies each copy
# byte-identical to the source. Fails loudly on any mismatch.
stage_license_files() {
    local dest="$1"
    mkdir -p "$dest/docs"
    for rel in "${LICENSE_FILES[@]}"; do
        local src="$REPO_ROOT/$rel"
        local dst="$dest/$rel"
        if [ ! -f "$src" ]; then
            echo "ERROR: license source missing: $src" >&2
            return 1
        fi
        mkdir -p "$(dirname "$dst")"
        cp "$src" "$dst"
        cmp -s "$src" "$dst" || {
            echo "ERROR: staged license file differs from source: $rel" >&2
            return 1
        }
    done
    echo "license files staged: ${#LICENSE_FILES[@]} (byte-identical)"
}

# check_no_character_assets <dir>
# G3 negative check: no Mint character image/video/look/voice asset may
# be inside an Apache 2.0 artifact. docs/character-license.md is a
# license document, not an asset, so it is exempt. Fails on any hit.
check_no_character_assets() {
    local dir="$1"
    local hits
    hits=$(cd "$dir" && find . \
        \( -iname '*mint-thinking*' -o -iname '*mint-face*' \
           -o -iname '*.gif' -o -iname '*.mp4' \
           -o -path './looks/*' -o -iname '*look-*.png' \
           -o -iname '*voice*' \) \
        -not -path './docs/character-license.md' -print 2>/dev/null || true)
    if [ -n "$hits" ]; then
        echo "ERROR: character asset(s) leaked into artifact:" >&2
        echo "$hits" >&2
        return 1
    fi
    echo "character-asset check: 0 hits"
}

# secret_scan_dir <dir>
# Scans text files (and binary strings) for high-signal secret patterns.
# 0 hits required. Known test sentinels live only in #[cfg(test)] code
# and must never appear in a release artifact.
secret_scan_dir() {
    local dir="$1"
    local pattern='sk-ant-[A-Za-z0-9_-]{8,}|ghp_[A-Za-z0-9]{20,}|gho_[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16}|-----BEGIN (RSA |EC |OPENSSH )?PRIVATE KEY-----|sk-[A-Za-z0-9]{20,}'
    local hits
    hits=$(grep -rE -I --exclude-dir=.git "$pattern" "$dir" 2>/dev/null || true)
    if [ -n "$hits" ]; then
        echo "ERROR: secret pattern hit(s) in artifact:" >&2
        echo "$hits" >&2
        return 1
    fi
    # Same patterns inside binaries via strings(1).
    local bin_hits=""
    while IFS= read -r -d '' bin; do
        if strings "$bin" 2>/dev/null | grep -qE "$pattern"; then
            bin_hits="$bin_hits$bin"$'\n'
        fi
    done < <(find "$dir" -type f -perm -u+x -print0 2>/dev/null)
    if [ -n "$bin_hits" ]; then
        echo "ERROR: secret pattern hit(s) inside binary(ies):" >&2
        echo "$bin_hits" >&2
        return 1
    fi
    echo "secret scan: 0 hits"
}

# write_dist_readme <dest-dir> <platform-label> <binary-list...>
# Short human-readable note shipped inside the artifact.
write_dist_readme() {
    local dest="$1"; shift
    local platform="$1"; shift
    {
        echo "Mimint $platform — release artifact"
        echo
        echo "Binaries in bin/:"
        for b in "$@"; do echo "  - $b"; done
        echo
        echo "License: code is Apache 2.0 (see LICENSE). The Mint character"
        echo "(all images, looks, video, avatar face, design and the name"
        echo "as attached to the character) is NOT under Apache 2.0: it is"
        echo "covered by the additional conditions in"
        echo "docs/character-license.md. Do not edit, reuse, redistribute"
        echo "or derive from character assets without written permission."
        echo "No character asset is shipped in this artifact."
        echo
        echo "Third-party notices: THIRD-PARTY-NOTICES.md"
    } > "$dest/README.txt"
}
