#!/bin/bash
# Test script widget that speaks the cmdash frame protocol.
# Responds to FRAME requests with ANSI-styled text output.
# Used by integration tests in wiring_smoke.rs.

while IFS= read -r line; do
    # Trim whitespace
    line=$(echo "$line" | tr -d '\r')

    # Only respond to FRAME requests
    case "$line" in
        FRAME*)
            # Parse width and height from FRAME request
            width=80
            height=24
            for part in $line; do
                case "$part" in
                    width=*) width="${part#width=}" ;;
                    height=*) height="${part#height=}" ;;
                esac
            done

            # Output FRAME response header
            echo "FRAME width=$width height=$height"

            # Output ANSI-styled text lines
            echo -e "\033[1m\033[32mTest Widget Output\033[0m"
            echo "  Line 1: Hello from script widget"
            echo "  Line 2: Width=$width Height=$height"
            echo ""
            echo -e "\033[1m\033[33mStatus\033[0m"
            echo "  PID: $$"
            echo "  Protocol: v1"
            ;;
        KEY*)
            # Silently consume KEY messages (no response needed for now)
            ;;
        RESIZE*)
            # Silently consume RESIZE messages
            ;;
        FOCUS*)
            # Silently consume FOCUS messages
            ;;
    esac
done
