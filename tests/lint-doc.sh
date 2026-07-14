#!/usr/bin/env bash
# Runtime smoke test for the `just lint-doc` + `just lint-doc-family-strict`
# recipes. Pins: recipe enumeration, exit-0 on current codebase, and
# the byte-exact form of the `-D clippy::<lint-name>` arg shape.
#
# Tests:
#   1. Both recipes enumerate via `just --show`.
#   2. Both recipes exit 0 on the current codebase.
#   3. `-D clippy::doesnotexist_marker_xyz` produces an `unknown lint`
#      warning, pinning the arg shape.
#
# Requirements:
#   - `just` (https://github.com/casey/just), version 1.55+
#   - `cargo` + `rustc`
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

echo "== tests/lint-doc.sh =="
echo "project root: $PROJECT_ROOT"

for recipe in lint-doc lint-doc-family-strict lint-doc-numbering; do
    echo "--- just --show $recipe ---"
    if ! just --show "$recipe" > /tmp/lint-doc-show.txt; then
        echo "::FAIL:: just --show $recipe returned non-zero"
        exit 1
    fi
    if ! grep -qE "^${recipe}:" /tmp/lint-doc-show.txt; then
        echo "::FAIL:: just --show $recipe did not emit a $recipe: header line"
        cat /tmp/lint-doc-show.txt
        exit 1
    fi
    echo "  --show $recipe: OK"
done

for recipe in lint-doc lint-doc-family-strict lint-doc-numbering; do
    echo "--- $recipe (runtime) ---"
    OUT_FILE=/tmp/lint-doc-${recipe}.out.txt
    if ! just "$recipe" 2>&1 | tee "$OUT_FILE" | grep -q '^== PASS:'; then
        echo "::FAIL:: recipe $recipe did not emit == PASS: marker; output:"
        cat "$OUT_FILE"
        exit 1
    fi
    echo "  $recipe: PASS marker present"
done

echo "--- -D arg typo tripwire ---"
TYPO_OUT=$(cargo clippy --workspace --all-targets -- -D clippy::doesnotexist_marker_xyz 2>&1) || true
if ! echo "$TYPO_OUT" | grep -qE 'unknown lint.*doesnotexist_marker_xyz|warning\[E0602\]'; then
    echo "::FAIL:: -D-arg typo tripwire did NOT fire on a known-bad clippy arg"
    echo "$TYPO_OUT" | head -10
    exit 1
fi
echo "  -D arg typo: tripwire fires on unknown lint pattern OK"

echo ""
echo "== tests/lint-doc.sh PASS =="
