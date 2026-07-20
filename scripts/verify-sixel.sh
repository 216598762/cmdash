#!/usr/bin/env bash
# scripts/verify-sixel.sh — Manual Sixel fallback verification.
#
# Builds cmdash, launches it inside each requested terminal with the Sixel
# graphics path forced, and checks whether valid Sixel DCS sequences
# appear on the host terminal output.
#
# Usage:
#   ./scripts/verify-sixel.sh [terminal...]
#
# Examples:
#   ./scripts/verify-sixel.sh              # tries xterm, mlterm, foot
#   ./scripts/verify-sixel.sh xterm foot  # verify only xterm and foot
#
# Requirements:
#   - cargo
#   - at least one of xterm, mlterm, or foot installed
#   - script(1) for output capture (usually in util-linux on Linux/BSD)
#
# The script exits 0 if all requested terminals pass, 1 if any fail or if
# no terminal is available.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CMDASH_BIN="$PROJECT_ROOT/target/debug/cmdash"

# Terminals to verify, in order of preference.
DEFAULT_TERMINALS=(xterm mlterm foot)
TERMINALS=("${@:-${DEFAULT_TERMINALS[@]}}")

# Build cmdash once.
echo "Building cmdash..."
cargo build --bin cmdash --quiet

# Temporary directory for configs, emitters, and captured logs.
TMPDIR="$(mktemp -d /tmp/cmdash-sixel-verify.XXXXXX)"
trap 'rm -rf "$TMPDIR"' EXIT

EMITTER="$TMPDIR/emit-kitty.sh"
CONFIG="$TMPDIR/sixel-test.kdl"

# 1x1 red PNG, base64-encoded. Kitty graphics load command will be emitted
# by the child PTY; cmdash should intercept it and re-encode as Sixel for
# the host terminal.
RED_PNG="iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="

cat > "$EMITTER" <<EOF
#!/usr/bin/env bash
# Emit a tiny kitty graphics load command, then sleep so the frame is drawn.
printf '\\e_Gf=100,i=1,I=1,s=1,v=1,a=t;${RED_PNG}\\e\\\\'
sleep 2
EOF
chmod +x "$EMITTER"

cat > "$CONFIG" <<EOF
// Force Sixel graphics and run a pane that emits a kitty graphics command.
layout {
    pane kind=shell label="sixel-test" command="$EMITTER"
}
EOF

# Check whether a valid Sixel DCS sequence is present in the captured log.
# Sixel data streams begin with ESC P (DCS) and end with ESC \\ (ST).
has_sixel() {
    local file="$1"
    # Look for DCS introducer followed by Sixel introducer 'q'.
    grep -q $'\eP.*q' "$file" 2>/dev/null
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
    # Build the command that runs inside the terminal.
    # We use script(1) to capture the pty output to a log file.
    # CMDASH_GRAPHICS=sixel forces the Sixel path regardless of TERM.
    local run_cmd
    run_cmd="script -q '$log' -c 'CMDASH_GRAPHICS=sixel TERM=$term $CMDASH_BIN --config=$CONFIG'"

    case "$term" in
        xterm)
            # xterm -e runs the command; -fa and -fs just make the window
            # readable for a human who happens to be watching.
            xterm -fa 'monospace' -fs 10 -e sh -c "$run_cmd" &
            pid=$!
            ;;
        mlterm)
            mlterm -e sh -c "$run_cmd" &
            pid=$!
            ;;
        foot)
            foot sh -c "$run_cmd" &
            pid=$!
            ;;
        *)
            echo "    SKIP: $term is not one of the known Sixel terminals"
            return 2
            ;;
    esac

    # Wait long enough for cmdash to start, the child to emit the kitty
    # command, and the Sixel frame to be written.
    sleep 4

    # Gracefully terminate the terminal.
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true

    if [[ ! -f "$log" ]]; then
        echo "    FAIL: no output log captured from $term"
        return 1
    fi

    if has_sixel "$log"; then
        echo "    PASS: $term emitted Sixel DCS sequences"
        return 0
    else
        echo "    FAIL: $term did not emit Sixel DCS sequences (see $log)"
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
    echo "No supported Sixel terminals were available. Install xterm, mlterm, or foot."
    exit 1
fi

if [[ "$any_failed" -ne 0 ]]; then
    echo ""
    echo "One or more terminals failed Sixel verification."
    echo "Inspect the captured logs in $TMPDIR before the trap removes them."
    exit 1
fi

echo ""
echo "All requested terminals passed Sixel fallback verification."
exit 0
