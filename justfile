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
#   heredity /non-GNU-bash runner hosts too.
# - `cargo` + `rustc` (via the existing rustup toolchain on this host).
# - `jq` (for JSON parsing in the LLM-judge layer added atop `1b635fc`).

set shell := ["bash", "-u"]


# ------------------------------------------------------------------------------
# flake-soak: 100-iteration SOAK on the 3 newly-un-ignored kitty tests
#             WITH GPT-4.1-MINI LLM-JUDGE LAYER (added atop `1b635fc`).
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
# Local baseline: 30/30 green (3 tests x 10 iter) per `2feff0f`/`ecfa1f2`
# development on this Arch Linux PTY-alloc host. The 300-run sweep here
# is the wider gate narrowing flake-detection shape: any failure that
# wasn't caught in the 30-run baseline has a 10x better chance of
# surfacing here.
#
# LLM-judge layer (added atop `1b635fc`):
#   For each of the 300 cargo-test runs (100 iterations x 3 tests), the
#   captured cargo-test stdout is classified by `gpt-4.1-mini` as exactly
#   one of:
#     - `clean`: cargo test passed AND no warnings / deprecations / stderr
#       noise.
#     - `messy`: cargo test passed BUT stdout contains warnings,
#       deprecations, or other non-fatal noise.
#     - `troll`: cargo test FAILED OR stdout is empty / unparseable /
#       malformed.
#   The signal ratio `clean:messy:troll` is recorded at `soak-output.log`
#   so future audit-cycle readers can grep the artifact. The log is
#   structured for reproducibility:
#     header: `# flake-soak start_time=<ISO8601> model=gpt-4.1-mini total_iters=100 tests=3 sample_size=300`
#     per-run line: `iter=NNN test=<test> cargo_result=<pass|fail> llm_class=<clean|messy|troll> llm_ms=<elapsed>`
#     footer: `SOAK_COMPLETE cargo_pass=N clean=N messy=N troll=N sig_ratio=N/300:N/300:N/300`
#   Requires the `OPENAI_API_KEY` env var on the host. If unset, fail-fast
#   BEFORE the loop starts. Set it via `export OPENAI_API_KEY=sk-...` or
#   add to `~/.config/cmdash/env` (then `source` before invoking `just`).
#
# Fail-fast behaviour: the recipe `exit 1`s on the first FAILED cargo
# test, captures the full cargo test output via `tail -20` for
# forward-fixup `#[ignore = "..."]` reason text, prints per-test
# runtime in milliseconds using `date +%s%N` (POSIX-shell-portable).
[group('soak')]
flake-soak:
    #!/usr/bin/env bash
    set -euo pipefail
    # LLM-judge pre-flight: fail-fast on missing API key.
    if [ -z "${OPENAI_API_KEY:-}" ]; then
        echo "::FAIL:: OPENAI_API_KEY env var unset. The LLM-judge layer cannot run."
        echo "::FAIL:: Set it via: export OPENAI_API_KEY=sk-..." >&2
        exit 1
    fi
    # Counters + log-file setup (added atop `1b635fc`).
    SOAK_LOG=soak-output.log
    CLEAN=0
    MESSY=0
    TROLL=0
    echo "# flake-soak start_time=$(date -u +%Y-%m-%dT%H:%M:%SZ) model=gpt-4.1-mini total_iters=100 tests=3 sample_size=300" > "$SOAK_LOG"
    # System prompt for the LLM-judge classification.
    SYSTEM_MSG='Return JSON with classification field set to exactly one of: clean | messy | troll. clean=cargo test passed AND no warnings, deprecations, or stderr noise. messy=cargo test passed BUT stdout contains warnings/deprecations/non-fatal noise. troll=cargo test FAILED OR stdout is empty/unparseable/malformed.'
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
    # LLM-classification helper (added atop `1b635fc`). Returns a
    # single-word echo: clean | messy | troll. Failure-mode handling:
    #   - curl network drop: exponential backoff (2s, 4s, 8s, 16s)
    #     up to 4 attempts.
    #   - HTTP 401 (invalid key): fail-fast (no retry).
    #   - HTTP 429 / 5xx: same exponential backoff.
    #   - bad LLM response (no clean/messy/troll word): classify as
    #     `troll` defensively.
    classify_output() {
        local text="$1"
        local attempt=1 max_attempts=4 pause=2
        local classified="" tmpfile text_json response http_code body
        # Truncate text and escape as JSON string (bash 3.2+/jq).
        text_json=$(printf '%s' "$text" | head -c 2000 | jq -Rs .)
        tmpfile=$(mktemp)
        cat > "$tmpfile" << PAYLOAD_EOF
{
  "model": "gpt-4.1-mini",
  "response_format": {"type": "json_object"},
  "messages": [
    {"role": "system", "content": $(printf '%s' "$SYSTEM_MSG" | jq -Rs .)},
    {"role": "user", "content": $text_json}
  ],
  "temperature": 0,
  "max_tokens": 12
}
PAYLOAD_EOF
        while [ $attempt -le $max_attempts ]; do
            response=$(curl -sS --max-time 15 -w "\n__HTTP_STATUS__:%{http_code}\n" \
                -X POST "https://api.openai.com/v1/chat/completions" \
                -H "Authorization: Bearer $OPENAI_API_KEY" \
                -H "Content-Type: application/json" \
                --data-binary "@$tmpfile" 2>/dev/null) || {
                attempt=$((attempt+1))
                sleep "$pause"
                pause=$((pause * 2))
                continue
            }
            http_code=$(echo "$response" | grep -o '__HTTP_STATUS__:[0-9]*' | cut -d: -f2)
            body=$(echo "$response" | sed '/__HTTP_STATUS__:/d')
            if [ "$http_code" = "401" ]; then
                echo "::FAIL:: OPENAI_API_KEY returned HTTP 401 (invalid or revoked)" >&2
                rm -f "$tmpfile"
                exit 1
            fi
            if [ "$http_code" = "429" ] || [ "$http_code" = "500" ] || [ "$http_code" = "502" ] || [ "$http_code" = "503" ] || [ "$http_code" = "504" ]; then
                attempt=$((attempt+1))
                sleep "$pause"
                pause=$((pause * 2))
                continue
            fi
            if [ "$http_code" = "200" ]; then
                classified=$(echo "$body" | jq -r '.choices[0].message.content' 2>/dev/null | grep -Eo 'clean|messy|troll' | head -1)
                [ -z "$classified" ] && classified="troll"
                rm -f "$tmpfile"
                echo "$classified"
                return 0
            fi
            attempt=$((attempt+1))
            sleep "$pause"
            pause=$((pause * 2))
        done
        rm -f "$tmpfile"
        echo "troll"
    }
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
            CARGO_RESULT="fail"
            if echo "$OUT" | grep -q 'test result: ok'; then
                PASS=$((PASS+1))
                # `PASS_PER_TEST[$i]++` post-increments from 0 -> 1,
                # 1 -> 2, ..., 99 -> 100, all of which are truthy in
                # `(( expr ))` arithmetic context. Safe under `set -e`
                # without the `|| true` shield that the
                # `MS_PER_TEST_TOTAL += ELAPSED_MS` line above needs.
                (( PASS_PER_TEST[$i]++ ))
                CARGO_RESULT="pass"
            fi
            # LLM-judge layer (added atop `1b635fc`): classify even on
            # cargo FAIL so the signal ratio gets recorded even when
            # the harness fail-fast exits early.
            LLM_START_NS=$(nano_time)
            CLASS=$(classify_output "$OUT")
            LLM_END_NS=$(nano_time)
            LLM_MS=$(( (LLM_END_NS - LLM_START_NS) / 1000000 ))
            echo "iter=$n test=$T cargo_result=$CARGO_RESULT llm_class=$CLASS llm_ms=$LLM_MS" >> "$SOAK_LOG"
            case "$CLASS" in
                clean) CLEAN=$((CLEAN+1)) ;;
                messy) MESSY=$((MESSY+1)) ;;
                troll) TROLL=$((TROLL+1)) ;;
                *)     TROLL=$((TROLL+1)) ;;
            esac
            if [ "$CARGO_RESULT" != "pass" ]; then
                echo "::FLAKE:: iter=$n test=$T elapsed=${ELAPSED_MS}ms llm_class=$CLASS"
                echo "::FLAKE:: FAIL-OUTPUT-BEGIN"
                echo "$OUT" | tail -20
                echo "::FLAKE:: FAIL-OUTPUT-END"
                echo "::FLAKE:: Forward-fixup #[ignore = \"...\"] reason text: see cargo test output above."
                echo "::FLAKE:: Note: this recipe's exit-1 IS a forward-fixup candidate trigger."
                exit 1
            fi
        done
        [ $((n % 10)) -eq 0 ] && echo "PROGRESS iter=$n PASS=$PASS CLEAN=$CLEAN MESSY=$MESSY TROLL=$TROLL"
    done
    echo ""
    echo "== flake-soak SUMMARY (with LLM-judge layer) =="
    echo "iterations=100 tests=3 total_runs_cargo=$((100 * ${#TESTS[@]})) cargo_pass=$PASS clean=$CLEAN messy=$MESSY troll=$TROLL"
    echo "SIG_RATIO: clean=$CLEAN/300 messy=$MESSY/300 troll=$TROLL/300"
    EXPECTED_TOTAL=$((100 * ${#TESTS[@]}))
    # Active strict-pin bail-out (added atop `6acdd54`'s followup): catches
    # the visible-but-passing-troll gap. The loop's cargo FAIL branch
    # already exit-1's if cargo_pass < 300, so reaching this point implies
    # cargo_pass == EXPECTED_TOTAL; the LLM-judge sub-assertion
    # `clean+messy == EXPECTED_TOTAL` (i.e. NO troll) is what's tightened
    # here.
    if [ "$PASS" -ne "$EXPECTED_TOTAL" ] || [ "$((CLEAN + MESSY))" -ne "$EXPECTED_TOTAL" ]; then
        echo "::FAIL:: flake-soak STRICT-PIN tripped -- cargo_pass=$PASS expected=$EXPECTED_TOTAL; clean+messy=$((CLEAN + MESSY)) expected=$EXPECTED_TOTAL"
        echo "::FAIL:: forward-fixup candidate: see SIG_RATIO + per-run lines in soak-output.log for which iterations crossed into troll territory."
        echo "SOAK_FAIL_STRICT_PIN cargo_pass=$PASS clean=$CLEAN messy=$MESSY troll=$TROLL" >> "$SOAK_LOG"
        exit 1
    fi
    echo "== flake-soak PASS at the strict-pin: cargo_pass=$EXPECTED_TOTAL AND clean+messy=$EXPECTED_TOTAL =="
    echo "SOAK_COMPLETE cargo_pass=$PASS clean=$CLEAN messy=$MESSY troll=$TROLL sig_ratio=$CLEAN/300:$MESSY/300:$TROLL/300" >> "$SOAK_LOG"


# ------------------------------------------------------------------------------
# clippy-baseline-0: pin cargo clippy residual count to EXACTLY 0.
# ------------------------------------------------------------------------------
#
# Per the B1 forward-fixup atop `5754742` (the 1.0 checklist B1 line
# item), the `clippy-baseline-3` recipe (which hard-coded
# `EXPECTED=3` as a deliberate tripwire against the `5e27556`
# "3-residual" claim carried in earlier commit bodies) was renamed +
# retargeted to `clippy-baseline-0` with `EXPECTED=0` to match the
# current actual residual count on origin/main (which has been 0
# since the cleanup-era atoms resolved the 5e27556 residuals).
#
# The strict-pin intent is preserved: this recipe exits-1 if `cargo
# clippy` produces ANY residual `^error` line, so a future regression
# that introduces (or re-introduces) clippy warnings-as-errors still
# surfaces as a forward-fixup candidate. The tripwire no longer
# fires on first run (actual = expected = 0); it now fires only on
# regression.
#
# Baseline transition: `clippy-baseline-3` (tripwire at EXPECTED=3
# vs actual=0, deliberately failing) -> `clippy-baseline-0`
# (strict-pin at EXPECTED=0 vs actual=0, green on first run; will
# exit-1 only on regression to actual>0).
#
# The expected count is hard-coded to 0 (per the recipe's
# strict-pin option + the user's B1 release-time preference for
# `clippy-baseline-0` over documenting the tripwire shape in
# release notes). If the actual count drifts, the recipe's stdout
# shows:
#   EXPECTED count: 0
#   ACTUAL   count: <N>
#   ::FAIL:: clippy-baseline-0 strict-pin tripped
#   ::FAIL:: first-10-error-snippets-begin
#   ... (verbatim clippy error snippets) ...
#   ::FAIL:: first-10-error-snippets-end
#
# Forward-fixup candidates when this recipe fails:
#   - If ACTUAL>0: forward-fixup `clippy-residual-sweep` atom to
#     resolve the new residual(s); raise this recipe's EXPECTED
#     only if the residual is documented as a known limitation.
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
        echo ""
        echo "Forward-fixup candidate: re-derive the expected count OR fix the discrepancy."
        echo "Strict-pin: any drift in either direction trips the alert. FAIL exit 1."
        exit 1
    fi
    echo ""
    echo "== PASS: clippy-baseline-0 strict-pin holds at EXPECTED=ACTUAL=$COUNT =="
