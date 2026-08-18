#!/bin/sh
# Streaming log-tail widget. Stream mode keeps reading stdout as it arrives.
# Set CMDASH_LOG_FILE to the file to tail (defaults to /dev/null).
file="${CMDASH_LOG_FILE:-/dev/null}"
tail -n 20 -F "$file" 2>/dev/null
