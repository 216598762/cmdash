# cmdash local CI helper recipes.
#
# These recipes pin baselines established by the forward-fixup chain
# (apex: `7b65b7a`+ `.github/workflows/ci.yml`) so any local drift
# triggers a forward-fixup candidate atom. Designed to run on this
# PTY-alloc host AND on any future dev/CI image reproducibly.
#
# Conventions:
# - Strict mode (`set -euo pipefail`) in every recipe shell-script.
# - Print progress at configurable intervals (`PROGRESS` lines every
#   10 iters).
# - Fail-fast with clear failure-shape capture on first failure so
#   forward-fixup `#[ignore = "..."]` reason text has the precise
#   failing shape (cargo test panic / `left == right`).
# - Assertions print EXPECTED and ACTUAL on every tripwire so the
#   drift is obvious from the stdout alone.
#
# Run `just --list` to enumerate all recipes.

set shell := ["bash", "-cu"]

# ------------------------------------------------------------------------------
# flake-soak: 100-iteration SOAK on the 3 newly-un-ignored kitty tests.
# ------------------------------------------------------------------------------
#
# Origin: atom `eea5878` (chore atop `f158ea0`) flipped these 3 tests
# from `#[ignore]` to plain `#[test]`:
#   - `pty_kitty_load_emits_event_via_vte_hook`
#   - `pty_kitty_split_chunk_across_advances`
#   - `pty_kitty_place_command_emits_event`
#
# Plus atom `d060198` also un-`#[ignore]`-ed `pty_kitty_delete_emits_event`
# but the user's request asks for the 3 from `eea5878`'s set, so we
# stay strictly on those 3.
#
# Local baseline: 30/30 green (3 tests × 10 iter) per `2feff0f`/`ecfa1f2`
# development on this Arch Linux PTY-alloc host. The 300-run sweep here
# is the wider gate narrowing flake-detection shape: any failure that
# wasn't caught in the 30-run baseline has a 10× better chance of
# surfacing here.
#
# Fail-fast behaviour: the recipe `exit 1`s on the first FAILED test,
# captures the full cargo test output via `tail -20` for forward-fixup
# `#[ignore = "..."]` reason text, prints per-test runtime in
# milliseconds using `date +%s%N` (POSIX-shell-portable via Bash).
[group('soak')]
flake-soak:
    #!/usr/bin/env bash
    set -euo pipefail
    PASS=0
    declare -A PASS_PER_TEST=()
    declare -A MS_PER_TEST_TOTAL=()
    TESTS=(
        pty_kitty_load_emits_event_via_vte_hook
        pty_kitty_split_chunk_across_advances
        pty_kitty_place_command_emits_event
    )
    for T in "${TESTS[@]}"; do
        PASS_PER_TEST[$T]=0
        MS_PER_TEST_TOTAL[$T]=0
    done
    for n in $(seq 1 100); do
        for T in "${TESTS[@]}"; do
            START_NS=$(date +%s%N)
            OUT=$(cargo test -p cmdash-pty --test round_trip --quiet -- "$T" 2>&1) || true
            END_NS=$(date +%s%N)
            ELAPSED_MS=$(( (END_NS - START_NS) / 1000000 ))
            MS_PER_TEST_TOTAL[$T]=$(( ${MS_PER_TEST_TOTAL[$T]} + ELAPSED_MS ))
            if echo "$OUT" | grep -q 'test result: ok'; then
                PASS=$((PASS+1))
                PASS_PER_TEST[$T]=$(( ${PASS_PER_TEST[$T]} + 1 ))
            else
                echo "::FLAKE:: iter=$n test=$T elapsed=${ELAPSED_MS}ms"
                echo "::FLAKE:: FAIL-OUTPUT-BEGIN"
                echo "$OUT" | tail -20
                echo "::FLAKE:: FAIL-OUTPUT-END"
                echo "::FLAKE:: Forward-fixup #[ignore = \"...\"] reason text: see cargo test output above."
                echo "::FLAKE:: Note: this recipe's exit-1 IS a forward-fixup candidate trigger."
                exit 1
            fi
        done
        [ $((n % 10)) -eq 0 ] && echo "PROGRESS iter=$n PASS=$PASS"
    done
    echo ""
    echo "== flake-soak SUMMARY =="
    echo "iterations=100 tests=3 total_runs=$((100 * ${#TESTS[@]})) PASS=$PASS"
    for T in "${TESTS[@]}"; do
        AVG_MS=$(( MS_PER_TEST_TOTAL[$T] / 100 ))
        PASS_PCT=$(( PASS_PER_TEST[$T] * 100 / 100 ))
        printf '  %-50s pass=%3d/100 (%3d%%)  avg_runtime=%5dms\n' \
               "$T" "${PASS_PER_TEST[$T]}" "$PASS_PCT" "$AVG_MS"
    done
    echo ""
    echo "== flake-soak PASS at the strict-pin: 300/300 green =="

# ------------------------------------------------------------------------------
# clippy-baseline-3: pin cargo clippy residual count to EXACTLY 3.
# ------------------------------------------------------------------------------
#
# Per user authorization (strict-pin option), this recipe FAILS
# IMMEDIATELY if the count is not exactly 3. The current actual count
# is 0 (per `cargo clippy --workspace --all-targets -- -D warnings` on
# `origin/main@7b65b7a`), so the recipe WILL FAIL on first run -- that
# is the deliberate tripwire alerting to the stale `5e27556`
# "3-residual" claim carried in earlier commit bodies. Future drift in
# either direction (someone fixes a residual and the count drops, OR
# someone regresses clippy and the count rises) trips this alert and
# surfaces as a forward-fixup candidate.
#
# The expected count is hard-coded to 3 (per user spec). If the actual
# count drifts, the recipe's stdout shows:
#   EXPECTED count: 3
#   ACTUAL   count: <N>
#   ::FAIL:: clippy-baseline-3 strict-pin tripped
#   ::FAIL:: first-10-error-snippets-begin
#   ... (verbatim clippy error snippets) ...
#   ::FAIL:: first-10-error-snippets-end
#
# Forward-fixup candidates when this recipe fails:
#   - If ACTUAL=0 (current): re-baseline with `clippy-baseline-0`
#     recipe and rename this one OR raise the expected to 3 and
#     intentionally re-introduce the 5e27556 residuals.
#   - If ACTUAL>3: forward-fixup `clippy-residual-sweep` atom.
[group('lint')]
clippy-baseline-3:
    #!/usr/bin/env bash
    set -euo pipefail
    EXPECTED=3
    echo "== clippy-baseline-3 =="
    echo "command: cargo clippy --workspace --all-targets -- -D warnings"
    OUT=$(cargo clippy --workspace --all-targets -- -D warnings 2>&1) || true
    COUNT=$(echo "$OUT" | grep -c '^error' || true)
    echo "EXPECTED count: $EXPECTED"
    echo "ACTUAL   count: $COUNT"
    if [ "$COUNT" -ne "$EXPECTED" ]; then
        echo ""
        echo "::FAIL:: clippy-baseline-3 strict-pin tripped"
        echo "::FAIL:: expected=$EXPECTED actual=$COUNT"
        echo "::FAIL:: first-10-error-snippets-begin"
        echo "$OUT" | grep '^error' | head -10 || true
        echo "::FAIL:: first-10-error-snippets-end"
        echo ""
        echo "Forward-fixup candidate: re-derive the expected count OR fix the discrepancy."
        echo "Strict-pin: any drift in either direction trips the alert. FAIL exit 1."
        exit 1
    fi
    echo ""
    echo "== PASS: clippy-baseline-3 strict-pin holds at EXPECTED=ACTUAL=$COUNT =="
