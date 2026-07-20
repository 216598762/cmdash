#!/usr/bin/env sh
# print-hyperlinks.sh — helper for examples/10-osc8-hyperlinks.kdl
#
# Prints a few lines with OSC 8 hyperlinks, then replaces itself
# with the user's default shell so the pane stays interactive.

# Open hyperlink to example.com, print label, then close hyperlink.
printf '\033]8;;https://example.com\033\\Hyperlink to example.com\033]8;;\033\\\n'

# Open hyperlink to cmdash.dev, print label, then close hyperlink.
printf '\033]8;;https://cmdash.dev\033\\cmdash project page\033]8;;\033\\\n'

# A standalone close-only hyperlink sequence (no URI).
printf 'Close-only sequence: \033]8;;\033\\\n'

# Hand off to the user's shell.
exec "${SHELL:-/bin/sh}"
