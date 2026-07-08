#!/usr/bin/env bash
# Regression test pinning the justfile parse correctness.
#
# Runs `just --list` + `just --show <recipe>` for every recipe in the
# justfile and asserts each call exits 0. If a future regression
# introduces a parse error, the recipe's `just --show` will fail.
#
# Requirements:
#   - `just` (https://github.com/casey/just), version 1.55+
#   - `grep`, `sed`, `awk` (POSIX-portable)
set -euo pipefail

JUSTFILE="${JUSTFILE_PATH:-justfile}"
if ! command -v just >/dev/null 2>&1; then
    echo "::FAIL:: \`just\` not found on PATH; install via \`cargo install just --locked\`" >&2
    exit 3
fi

echo "== just --list ($JUSTFILE) =="
if ! just --list --justfile "$JUSTFILE" > /tmp/justfile-list.txt 2> /tmp/justfile-list.err; then
    echo "::FAIL:: \`just --list\` returned non-zero; justfile parse error:" >&2
    cat /tmp/justfile-list.err >&2
    exit 1
fi
cat /tmp/justfile-list.txt

EXPECTED_RECIPES=(
    clippy-baseline-0
    gpg-setup
    lint-doc
    lint-doc-family-strict
)
for recipe in "${EXPECTED_RECIPES[@]}"; do
    if ! grep -qE "(^|[[:space:]])${recipe}([[:space:]]|$)" /tmp/justfile-list.txt; then
        echo "::FAIL:: expected recipe \`${recipe}\` not found in \`just --list\` output" >&2
        cat /tmp/justfile-list.txt >&2
        exit 1
    fi
done
echo "  -> all ${#EXPECTED_RECIPES[@]} expected recipes present in --list output"

for recipe in "${EXPECTED_RECIPES[@]}"; do
    echo "== just --show ${recipe} =="
    if ! just --show "$recipe" --justfile "$JUSTFILE" > /tmp/justfile-show.txt 2> /tmp/justfile-show.err; then
        echo "::FAIL:: \`just --show ${recipe}\` returned non-zero; recipe body parse error:" >&2
        cat /tmp/justfile-show.err >&2
        exit 1
    fi
    if ! grep -qE "^${recipe}:" /tmp/justfile-show.txt; then
        echo "::FAIL:: \`just --show ${recipe}\` did not emit a \`${recipe}:\` header line" >&2
        head -10 /tmp/justfile-show.txt >&2
        exit 1
    fi
    body_lines=$(grep -cvE '^\s*(#|$)' /tmp/justfile-show.txt || true)
    if [ "$body_lines" -lt 5 ]; then
        echo "::FAIL:: \`just --show ${recipe}\` body has only ${body_lines} non-comment lines (expected >= 5)" >&2
        cat /tmp/justfile-show.txt >&2
        exit 1
    fi
    echo "  -> ${recipe}: --show OK, body has ${body_lines} non-comment lines"
done

echo ""
echo "== justfile-parse regression test PASS =="
