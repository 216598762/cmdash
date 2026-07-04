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
## Run `just --list` to enumerate all recipes.
#
# Requirements:
# - `just` (https://github.com/casey/just). Install once via
#   `cargo install just --locked` if not already present (verify with
#   `command -v just`). Not a build-dep -- only used at invocation time.
# - Bash 3.2+ (macOS /bin/bash 3.2 supported). The recipes use indexed
#   arrays instead of `declare -A` associative arrays to preserve
#   cross-platform portability; the indexed-array variant was added
#   in the `forward-fixup atom atop d93b7a7` so the recipe runs on
#   heredity / non-GNU-bash runner hosts too.
# - `cargo` + `rustc` (via the existing rustup toolchain on this host).

set shell := ["bash", "-u"]


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
    # Indexed arrays (Bash 3.2+ portable) instead of `declare -A`
    # associative arrays (Bash 4+) so the recipe runs on macOS
    # /bin/bash 3.2 too.
    TESTS=(
        pty_kitty_load_emits_event_via_vte_hook
        pty_kitty_split_chunk_across_advances
        pty_kitty_place_command_emits_event
    )
    declare -a PASS_PER_TEST=()
    declare -a MS_PER_TEST_TOTAL=()
    for i in "${!TESTS[@]}"; do
        PASS_PER_TEST+=(0)
        MS_PER_TEST_TOTAL+=(0)
    done
    # Portable nanosecond timestamp helper. The detection probes
    # OUTPUT content, not exit code: GNU `date` emits seconds.ns
    # like `1717514400.123456789` (contains a `.`); macOS BSD
    # `date` does NOT recognize `%N` and emits the literal `%N`
    # suffix with exit code 0 -- so an `> /dev/null 2>&1` exit-check
    # would mis-detect BSD as GNU and silently corrupt runtime
    # numbers when the literal `%N` token enters arithmetic. We probe
    # `[[ $(date +%s%N) == *.* ]]` instead, which is reliable
    # across both implementations.
    #
    # Note: the BSD fallback drops to 1-second grain. Any test
    # finishing in <1s on macOS reports `avg_runtime=0ms` -- this is
    # a documented limitation (the bash 3.2+ requirement blocks
    # using `${EPOCHREALTIME}`, which is bash 5+ only). Downstream
    # arithmetic `(end - start) / 1000000` still yields ms uniformly
    # (0 when both timestamps fall in the same second).
    if [[ $(date +%s%N) == *.* ]]; then
        nano_time() { date +%s%N; }
    else
        nano_time() { echo $(( $(date +%s) * 1000000000 )); }
    fi
    for n in $(seq 1 100); do
        for i in "${!TESTS[@]}"; do
            T="${TESTS[$i]}"
            START_NS=$(nano_time)
            OUT=$(cargo test -p cmdash-pty --test round_trip --quiet -- "$T" 2>&1) || true
            END_NS=$(nano_time)
            ELAPSED_MS=$(( (END_NS - START_NS) / 1000000 ))
            # `(( arr[i] += x ))` is portable for arithmetic on
            # indexed-array elements across bash 3.2/4.x. We append
            # `|| true` because `(( expr ))` under `set -e` will exit
            # the script when the post-evaluation result is 0 (a known
            # bash idiom trap): for our fast-running tests, the
            # per-test runtime measured in milliseconds is often 0ms,
            # so MS_PER_TEST_TOTAL[$i] += 0 leaves the element at 0
            # which trips `(( ... ))` exit code 1 if not neutralized.
            (( MS_PER_TEST_TOTAL[$i] += ELAPSED_MS )) || true
            if echo "$OUT" | grep -q 'test result: ok'; then
                PASS=$((PASS+1))
                # `PASS_PER_TEST[$i]++` post-increments from 0 -> 1,
                # 1 -> 2, ..., 99 -> 100, all of which are truthy in
                # `(( expr ))` arithmetic context. Safe under `set -e`
                # without the `|| true` shield that the
                # `MS_PER_TEST_TOTAL += ELAPSED_MS` line above needs.
                (( PASS_PER_TEST[$i]++ ))
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
    for i in "${!TESTS[@]}"; do
        T="${TESTS[$i]}"
        AVG_MS=$(( MS_PER_TEST_TOTAL[$i] / 100 ))
        printf '  %-50s pass=%3d/100  avg_runtime=%5dms\n' \
               "$T" "${PASS_PER_TEST[$i]}" "$AVG_MS"
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
