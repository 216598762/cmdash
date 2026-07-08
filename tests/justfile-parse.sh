#!/usr/bin/env bash
# Regression test pinning the `justfile` parse fix.
#
# Background: the `flake-soak` recipe previously inlined a
# `$(printf '%s' "$SYSTEM_MSG" | jq -Rs .)` inside a heredoc body.
# `just` 1.55.1 parses recipe bodies token-by-token and choked on
# the `|` (then on JSON `0`/`:`s after the inline form was
# extracted to a variable). The fix replaces the heredoc with a
# `jq -nc --arg` invocation that keeps the JSON template in a
# single-quoted string so `just` sees a single token.
#
# This script runs `just --list` + `just --show <recipe>` for every
# recipe in the justfile and asserts each call exits 0. If a future
# regression re-introduces JSON-in-heredoc or other `just` parser
# tripwires, the recipe's `just --show` will fail and the script
# exits non-zero, failing CI.
#
# Requirements:
#   - `just` (https://github.com/casey/just), version 1.55+
#   - `grep`, `sed`, `awk` (POSIX-portable)
#
# This script does NOT execute any recipe -- it only verifies the
# justfile parses. Recipe runtime tests (e.g. `just clippy-baseline-0`)
# are out of scope here; they belong to their own recipes + CI runs.
set -euo pipefail

# The justfile under test is always the one in the repo root.
JUSTFILE="${JUSTFILE_PATH:-justfile}"
# `just` lives on PATH; if not, fail-fast with a clear message.
if ! command -v just >/dev/null 2>&1; then
    echo "::FAIL:: \`just\` not found on PATH; install via \`cargo install just --locked\`" >&2
    exit 3
fi

# Step 1: `just --list` must succeed. A parse error in the justfile
# trips a non-zero exit BEFORE listing any recipe.
echo "== just --list ($JUSTFILE) =="
if ! just --list --justfile "$JUSTFILE" > /tmp/justfile-list.txt 2> /tmp/justfile-list.err; then
    echo "::FAIL:: \`just --list\` returned non-zero; justfile parse error:" >&2
    cat /tmp/justfile-list.err >&2
    exit 1
fi
cat /tmp/justfile-list.txt

# Step 2: every recipe that the project ships must be enumerable in
# the `--list` output. The recipe name appears in column-4-aligned
# output (`    recipe_name    # comment`); match the recipe name as a
# whole word to avoid false-positives on substrings of other tokens.
EXPECTED_RECIPES=(
    flake-soak
    clippy-baseline-0
    gpg-setup
    lint-doc
    lint-doc-family
)
for recipe in "${EXPECTED_RECIPES[@]}"; do
    # The recipes are listed indented, so use a word-boundary match
    # that doesn't anchor on line-start. `just --list` aligns
    # recipe names at column 4 by default.
    if ! grep -qE "(^|[[:space:]])${recipe}([[:space:]]|$)" /tmp/justfile-list.txt; then
        echo "::FAIL:: expected recipe \`${recipe}\` not found in \`just --list\` output" >&2
        cat /tmp/justfile-list.txt >&2
        exit 1
    fi
done
echo "  -> all ${#EXPECTED_RECIPES[@]} expected recipes present in --list output"

# Step 3: `just --show <recipe>` must succeed for every recipe.
# `--show` parses the recipe body, so a parse error in the body
# surfaces here even if `--list` succeeded (the `--list` path
# walks recipe names only; `--show` walks the body).
for recipe in "${EXPECTED_RECIPES[@]}"; do
    echo "== just --show ${recipe} =="
    if ! just --show "$recipe" --justfile "$JUSTFILE" > /tmp/justfile-show.txt 2> /tmp/justfile-show.err; then
        echo "::FAIL:: \`just --show ${recipe}\` returned non-zero; recipe body parse error:" >&2
        cat /tmp/justfile-show.err >&2
        exit 1
    fi
    # Sanity: the dump must include the recipe-name line
    # (`just --show` outputs the recipe header as
    # `recipe-name:` on its own line, AFTER any preceding
    # comment lines + `[group(...)]` attributes).
    if ! grep -qE "^${recipe}:" /tmp/justfile-show.txt; then
        echo "::FAIL:: \`just --show ${recipe}\` did not emit a \`${recipe}:\` header line" >&2
        head -10 /tmp/justfile-show.txt >&2
        exit 1
    fi
    # Sanity: the body must contain at least the first non-comment,
    # non-blank line of the recipe. A `--show` that returns
    # successfully but with an empty body would be a regression.
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
echo "  just --list: OK"
echo "  expected recipes enumerated: ${EXPECTED_RECIPES[*]}"
echo "  just --show per recipe: OK"
