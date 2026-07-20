#!/usr/bin/env bash
# scripts/verify-kitty.sh — Manual Kitty graphics protocol verification.
#
# Builds cmdash, launches it inside each requested terminal with the Kitty
# graphics path forced, and checks whether valid Kitty APC-G sequences
# appear on the host terminal output.
#
# Usage:
#   ./scripts/verify-kitty.sh [terminal...]
#
# Examples:
#   ./scripts/verify-kitty.sh              # tries kitty, foot, wezterm
#   ./scripts/verify-kitty.sh kitty foot   # verify only kitty and foot
#
# Requirements:
#   - cargo
#   - at least one of kitty, foot, or wezterm installed
#   - script(1) for output capture (usually in util-linux on Linux/BSD)
#
# The script exits 0 if all requested terminals pass, 1 if any fail or if
# no terminal is available.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CMDASH_BIN="$PROJECT_ROOT/target/debug/cmdash"

# Terminals to verify, in order of preference.
DEFAULT_TERMINALS=(kitty foot wezterm)
TERMINALS=("${@:-${DEFAULT_TERMINALS[@]}}")

# Build cmdash once.
echo "Building cmdash..."
cargo build --bin cmdash --quiet

# Temporary directory for configs, emitters, and captured logs.
TMPDIR="$(mktemp -d /tmp/cmdash-kitty-verify.XXXXXX)"
trap 'rm -rf "$TMPDIR"' EXIT

EMITTER="$TMPDIR/emit-kitty.sh"
CONFIG="$TMPDIR/kitty-test.kdl"

# 1x1 red PNG, base64-encoded. Kitty graphics load command will be emitted
# by the child PTY; cmdash should forward it to the host terminal.
RED_PNG="iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="

cat > "$EMITTER" <<EOF
#!/usr/bin/env bash
# Emit a tiny kitty graphics load command, then sleep so the frame is drawn.
printf '\\e_Gf=100,i=1,I=1,s=1,v=1,a=t;${RED_PNG}\\e\\\\'
sleep 2
EOF
chmod +x "$EMITTER"

cat > "$CONFIG" <<EOF
// Force Kitty graphics and run a pane that emits a kitty graphics command.
layout {
    pane kind=shell label="kitty-test" command="$EMITTER"
}
EOF

# Check whether a valid Kitty APC-G sequence is present in the captured log.
# Kitty graphics streams begin with ESC _ G (APC "_G") and end with ESC \\.
has_kitty() {
    local file="$1"
    grep -q $'\e_G' "$file" 2>/dev/null
}

# Run cmdash inside a terminal and capture its output.
verify_terminal() {
    local term="$1"
    local log="$TMPDIR/cmdash-${term}.log"
    local pid

    echo ""
    echo "==> Verifying $term"

    if ! command -v "$term" &>/dev/null; then
        echo "    SKIP: $term is not installed"
        return 2
    fi

    # Build the command that runs inside the terminal.
    # We use script(1) to capture the pty output to a log file.
    local run_cmd
    run_cmd="script -q '$log' -c 'CMDASH_GRAPHICS=kitty TERM=xterm-kitty $CMDASH_BIN --config=$CONFIG'"

    case "$term" in
        kitty)
            # kitty -e runs the command.
            kitty -e sh -c "$run_cmd" &
            pid=$!
            ;;
        foot)
            foot sh -c "$run_cmd" &
            pid=$!
            ;;
        wezterm)
            wezterm start -- sh -c "$run_cmd" &
            pid=$!
            ;;
        *)
            echo "    SKIP: $term is not one of the known Kitty terminals"
            return 2
            ;;
    esac

    # Wait long enough for cmdash to start, the child to emit the kitty
    # command, and the Kitty frame to be written.
    sleep 4

    # Gracefully terminate the terminal.
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true

    if [[ ! -f "$log" ]]; then
        echo "    FAIL: no output log captured from $term"
        return 1
    fi

    if has_kitty "$log"; then
        echo "    PASS: $term emitted Kitty APC-G sequences"
        return 0
    else
        echo "    FAIL: $term did not emit Kitty APC-G sequences (see $log)"
        return 1
    fi
}

# Main loop.
echo "Temporary files are in: $TMPDIR"
echo "Config: $CONFIG"

any_failed=0
any_ran=0

for term in "${TERMINALS[@]}"; do
    if verify_terminal "$term"; then
        any_ran=1
    elif [[ $? -eq 1 ]]; then
        any_ran=1
        any_failed=1
    fi
done

if [[ "$any_ran" -eq 0 ]]; then
    echo ""
    echo "No supported Kitty terminals were available. Install kitty, foot, or wezterm."
    exit 1
fi

if [[ "$any_failed" -ne 0 ]]; then
    echo ""
    echo "One or more terminals failed Kitty verification."
    echo "Inspect the captured logs in $TMPDIR before the trap removes them."
    exit 1
fi

echo ""
echo "All requested terminals passed Kitty graphics verification."
exit 0
