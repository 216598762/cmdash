#!/usr/bin/env bash
# examples/graphics-test-emitter.sh — emit a tiny kitty graphics load command.
#
# Usage: examples/graphics-test-emitter.sh [sixel|kitty]
#
# This helper is used by the Sixel and Kitty verification examples.
# It prints a 1x1 red PNG via the kitty graphics protocol. cmdash
# intercepts the command and re-encodes it as Sixel or forwards it as
# Kitty graphics according to the host terminal's selected protocol.

set -euo pipefail

# The protocol argument is informational only; the emitted payload is the
# same kitty graphics load command in both cases.
PROTOCOL="${1:-sixel}"

# 1x1 red PNG, base64-encoded.
RED_PNG="iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="

# Kitty graphics load command:
#   f=100  PNG format
#   i=1    image id
#   I=1    image number (animation frame)
#   s=1    source width hint
#   v=1    source height hint
#   a=t    transmit and display immediately
printf '\e_Gf=100,i=1,I=1,s=1,v=1,a=t;%s\e\\' "$RED_PNG"

# Keep the pane alive briefly so the frame is rendered.
sleep 2
