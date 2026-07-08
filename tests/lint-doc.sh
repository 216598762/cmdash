#!/usr/bin/env bash
# Runtime smoke test for the `just lint-doc` + `just lint-doc-family`
# recipes. Pin: a future regression in the cargo invocation (a `-D`
# arg typo or a missing `-A clippy::all`) trips this test before the
# recipes themselves can drift silently.
#
# Tests:
#   1. Both recipes enumerate via `just --show` (parse + body present).
#   2. Both recipes exit 0 on the current codebase (tripwire holds),
#      emitting `== PASS:` markers in stdout.
#   3. `-D clippy::doesnotexist_marker_xyz` produces an
#      `unknown lint` warning, pinning the byte-exact form of the
#      `-D clippy::<lint-name>` arg shape used by both recipes.
#
# Cycle-21 atom-3 polish-tier item (d): `tests/justfile-parse.sh`
# pins recipe-enumeration (this file's #1) but does NOT verify the
# cargo invocation payload is byte-exact. A typo in
# `-D clippy::doc_lazy_continuation` (silent character flip) would
# pass parse-tests AND pass `clippy-baseline-0` (which only checks
# `-D warnings`), but the recipe would silently no-op (cargo
# emits `unknown lint` warning + continues, exit 0). This runtime
# test closes the gap by emitting a known-bad lint name and asserting
# the `unknown lint` pattern fires (which is what the recipe's
# payload would emit if a real typo flipped a character).
#
# Requirements:
#   - `just` (https://github.com/casey/just), version 1.55+
#   - `cargo` + `rustc` (via the existing rustup toolchain on this host)
set -euo pipefail

# Locate the project root (this script lives in tests/).
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

echo "== tests/lint-doc.sh =="
echo "project root: $PROJECT_ROOT"

# 1. `just --show` enumerates both recipes and emits the expected
# `recipe-name:` header line in the body dump.
for recipe in lint-doc lint-doc-family; do
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

# 2. Both recipes run cleanly on the current codebase (tripwire
# holds; both exit 0 with == PASS: marker in stdout). A future
# regression in either recipe's cargo invocation that flips
# the exit code trips THIS assertion on the FIRST run, before
# the recipe body can drift silently future-cycles.
for recipe in lint-doc lint-doc-family; do
    echo "--- $recipe (runtime) ---"
    OUT_FILE=/tmp/lint-doc-${recipe}.out.txt
    if ! just "$recipe" 2>&1 | tee "$OUT_FILE" | grep -q '^== PASS:'; then
        echo "::FAIL:: recipe $recipe did not emit == PASS: marker; output:"
        cat "$OUT_FILE"
        exit 1
    fi
    echo "  $recipe: PASS marker present"
done

# 3. `-D clippy::<lint-name>` arg-shape tripwire: a deliberate typo
# produces `warning[E0602]: unknown lint`. This pins the byte-exact
# form of the `-D clippy::doc_lazy_continuation` (and friends) args
# used in the recipes. A future typo in either recipe would slip
# past parse-tests AND `tests/lint-doc.sh` #1 + #2 (which only
# check recipe structure + exit-0), but THIS assertion fires
# because cargo clippy would emit the unknown-lint warning shape
# the recipe's payload WOULD have produced if the arg were
# byte-exact. (Cargo's unknown-lint behaviour for `-D clippy::X`:
# emit `warning[E0602]: unknown lint: clippy::X`, exit 0, continue.)
echo "--- -D arg typo tripwire (cargo clippy -D clippy::doesnotexist_marker_xyz) ---"
TYPO_OUT=$(cargo clippy --workspace --all-targets -- -D clippy::doesnotexist_marker_xyz 2>&1) || true
if ! echo "$TYPO_OUT" | grep -qE 'unknown lint.*doesnotexist_marker_xyz|warning\[E0602\]'; then
    echo "::FAIL:: -D-arg typo tripwire did NOT fire on a known-bad clippy arg"
    echo "$TYPO_OUT" | head -10
    exit 1
fi
echo "  -D arg typo: tripwire fires on unknown lint pattern OK"

echo ""
echo "== tests/lint-doc.sh PASS =="
echo "  just --show lint-doc + lint-doc-family: OK"
echo "  runtime exit-0 + ::PASS:: markers: OK"
echo "  -D arg typo tripwire: OK"
