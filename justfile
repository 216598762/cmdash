# cmdash local CI helper recipes.
#
# Run `just --list` to enumerate all recipes.
#
# Requirements:
# - `just` (https://github.com/casey/just)
# - `cargo` + `rustc`
# - `jq` (not currently needed, but available for future recipes)

set shell := ["bash", "-u"]

# ------------------------------------------------------------------------------
# clippy-baseline-0: pin cargo clippy residual count to EXACTLY 0.
#
# Exits-1 if cargo clippy produces ANY residual `^error` line.
# Green on first run (actual = expected = 0); fires only on regression.
# ------------------------------------------------------------------------------
[group('lint')]
clippy-baseline-0:
    #!/usr/bin/env bash
    set -euo pipefail
    EXPECTED=0
    echo "== clippy-baseline-0 =="
    echo "command: cargo clippy --workspace --all-targets -- -D warnings"
    OUT=$(cargo clippy --workspace --all-targets -- -D warnings 2>&1) || true
    COUNT=$(echo "$OUT" | grep -c '^error' || true)
    echo "EXPECTED count: $EXPECTED"
    echo "ACTUAL   count: $COUNT"
    if [ "$COUNT" -ne "$EXPECTED" ]; then
        echo ""
        echo "::FAIL:: clippy-baseline-0 strict-pin tripped"
        echo "::FAIL:: expected=$EXPECTED actual=$COUNT"
        echo "::FAIL:: first-10-error-snippets-begin"
        echo "$OUT" | grep '^error' | head -10 || true
        echo "::FAIL:: first-10-error-snippets-end"
        exit 1
    fi
    echo ""
    echo "== PASS: clippy-baseline-0 strict-pin holds at EXPECTED=ACTUAL=$COUNT =="

# ------------------------------------------------------------------------------
# lint-doc: deny-only pin against `clippy::doc_lazy_continuation`.
#
# Fast targeted lint for editor save hooks. The full
# `cargo clippy --workspace --all-targets -- -D warnings` still runs
# in `clippy-baseline-0`.
# ------------------------------------------------------------------------------
[group('lint')]
lint-doc:
    #!/usr/bin/env bash
    set -euo pipefail
    EXPECTED=0
    echo "== lint-doc =="
    echo "command: cargo clippy --workspace --all-targets -- -A clippy::all -D clippy::doc_lazy_continuation"
    OUT=$(cargo clippy --workspace --all-targets -- -A clippy::all -D clippy::doc_lazy_continuation 2>&1) || true
    COUNT=$(echo "$OUT" | grep -c '^error' || true)
    echo "EXPECTED count: $EXPECTED"
    echo "ACTUAL   count: $COUNT"
    if [ "$COUNT" -ne "$EXPECTED" ]; then
        echo ""
        echo "::FAIL:: lint-doc tripwire fired"
        echo "::FAIL:: first-10-error-snippets-begin"
        echo "$OUT" | head -10
        echo "::FAIL:: first-10-error-snippets-end"
        exit 1
    fi
    echo ""
    echo "== PASS: lint-doc strict-pin holds at EXPECTED=ACTUAL=$COUNT =="

# ------------------------------------------------------------------------------
# lint-doc-family-strict: deny-only pin against the doc-lint family.
#
# Checks `clippy::doc_markdown` + `clippy::empty_line_after_doc_comments`.
# ------------------------------------------------------------------------------
[group('lint')]
lint-doc-family-strict:
    #!/usr/bin/env bash
    set -euo pipefail
    EXPECTED=0
    echo "== lint-doc-family-strict =="
    echo "command: cargo clippy --workspace --all-targets -- -A clippy::all -D clippy::doc_markdown -D clippy::empty_line_after_doc_comments"
    OUT=$(cargo clippy --workspace --all-targets -- -A clippy::all -D clippy::doc_markdown -D clippy::empty_line_after_doc_comments 2>&1) || true
    COUNT=$(echo "$OUT" | grep -c '^error' || true)
    echo "EXPECTED count: $EXPECTED"
    echo "ACTUAL   count: $COUNT"
    if [ "$COUNT" -ne "$EXPECTED" ]; then
        echo ""
        echo "::FAIL:: lint-doc-family-strict strict-pin tripped"
        echo "::FAIL:: expected=$EXPECTED actual=$COUNT"
        echo "::FAIL:: first-10-error-snippets-begin"
        echo "$OUT" | grep '^error' | head -10 || true
        echo "::FAIL:: first-10-error-snippets-end"
        exit 1
    fi
    echo ""
    echo "== PASS: lint-doc-family-strict strict-pin holds at EXPECTED=ACTUAL=$COUNT =="

# ------------------------------------------------------------------------------
# lint-doc-numbering: verify docs/configuration.md heading numbers are sequential.
# ------------------------------------------------------------------------------
[group('lint')]
lint-doc-numbering:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "== lint-doc-numbering =="
    python3 tests/check-doc-numbering.py docs/configuration.md
    echo ""
    echo "== PASS: lint-doc-numbering =="

# ------------------------------------------------------------------------------
# nextest: run the full workspace test suite via cargo-nextest.
#
# Faster than `cargo test` due to per-binary parallelism and
# process isolation. Requires `cargo-nextest` to be installed.
# ------------------------------------------------------------------------------
[group('test')]
nextest:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "== nextest =="
    echo "command: cargo nextest run --workspace"
    cargo nextest run --workspace
    echo ""
    echo "== PASS: nextest completed successfully =="

# ------------------------------------------------------------------------------
# doc-gate: deny broken intra-doc links in the cmdash lib crate.
#
# Uses RUSTDOCFLAGS because newer cargo versions do not forward
# bare `-D` flags from the cargo doc command line.
# ------------------------------------------------------------------------------
[group('lint')]
doc-gate:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "== doc-gate =="
    echo "command: RUSTDOCFLAGS='-D rustdoc::broken-intra-doc-links' cargo doc -p cmdash --lib --no-deps"
    RUSTDOCFLAGS='-D rustdoc::broken-intra-doc-links' cargo doc -p cmdash --lib --no-deps 2>&1
    echo ""
    echo "== PASS: doc-gate: no broken intra-doc links =="

# ------------------------------------------------------------------------------
# gpg-setup: wire git's gpg.program to scripts/gpg-cmdash-wrapper.sh and
# re-enable commit.gpgsign. Run once per host. The wrapper ships in the
# repo and contains no secrets; the passphrase is held host-local at
# ~/.config/cmdash/gpg-passphrase (chmod 600).
# ------------------------------------------------------------------------------
gpg-setup:
    @chmod 700 scripts/gpg-cmdash-wrapper.sh
    @git config --local gpg.program "$(pwd)/scripts/gpg-cmdash-wrapper.sh"
    @git config --local commit.gpgsign true
    @echo "== gpg-cmdash-wrapper configured =="
    @echo "Local git gpg.program:  $(git config --local gpg.program)"
    @echo "Local git commit.gpgsign: $(git config --local --get commit.gpgsign)"
    @echo "Next: seed your host-local passphrase file:"
    @echo "  mkdir -p ~/.config/cmdash"
    @echo "  printf '%s' 'YOUR_PASSPHRASE' > ~/.config/cmdash/gpg-passphrase"
    @echo "  chmod 600 ~/.config/cmdash/gpg-passphrase"
