#!/usr/bin/env python3
"""Example cmdash script widget — displays system uptime and load.

This script speaks the cmdash line-delimited frame protocol (v1).
It receives FRAME requests on stdin and responds with ANSI-styled
text on stdout.

Protocol flow:
  1. Host sends:  FRAME width=80 height=24 gen=1
  2. Script sends: FRAME width=80 height=24
                   <ANSI text lines>
  3. Repeat for each frame.

Usage in KDL config:
  pane kind=script command="python3 examples/widgets/uptime.py" label="uptime"

Build:
  chmod +x examples/widgets/uptime.py

Test standalone:
  echo "FRAME width=80 height=24 gen=1" | python3 examples/widgets/uptime.py
"""

import sys
import time


def read_uptime():
    """Read system uptime from /proc/uptime."""
    try:
        with open("/proc/uptime") as f:
            secs = float(f.read().split()[0])
        days = int(secs // 86400)
        hours = int((secs % 86400) // 3600)
        mins = int((secs % 3600) // 60)
        return f"{days}d {hours}h {mins}m"
    except Exception:
        return "N/A"


def read_load():
    """Read load average from /proc/loadavg."""
    try:
        with open("/proc/loadavg") as f:
            parts = f.read().split()
        return parts[0], parts[1], parts[2]
    except Exception:
        return "N/A", "N/A", "N/A"


def read_memory():
    """Read memory usage from /proc/meminfo."""
    try:
        with open("/proc/meminfo") as f:
            lines = f.readlines()
        total = int(lines[0].split()[1]) // 1024  # kB → MB
        avail = int(lines[2].split()[1]) // 1024
        used = total - avail
        pct = int(used * 100 / total) if total > 0 else 0
        return f"{used}M / {total}M ({pct}%)"
    except Exception:
        return "N/A"


def render_frame(width, height):
    """Render a single frame of output."""
    # ANSI escape codes
    BOLD = "\033[1m"
    DIM = "\033[2m"
    GREEN = "\033[32m"
    YELLOW = "\033[33m"
    CYAN = "\033[36m"
    RESET = "\033[0m"

    lines = []

    lines.append(f"{BOLD}{GREEN}System Uptime{RESET}")
    lines.append(f"  {read_uptime()}")
    lines.append("")

    l1, l5, l15 = read_load()
    lines.append(f"{BOLD}{YELLOW}Load Average{RESET}")
    lines.append(f"  1m: {l1}  5m: {l5}  15m: {l15}")
    lines.append("")

    lines.append(f"{BOLD}{CYAN}Memory{RESET}")
    lines.append(f"  {read_memory()}")
    lines.append("")

    ts = time.strftime("%Y-%m-%d %H:%M:%S")
    lines.append(f"{BOLD}Current Time{RESET}")
    lines.append(f"  {ts}")

    return lines


def main():
    while True:
        line = sys.stdin.readline()
        if not line:
            break  # EOF — host closed stdin

        line = line.strip()
        if not line.startswith("FRAME "):
            continue  # Skip non-FRAME lines

        # Parse FRAME request
        params = {}
        for part in line[6:].split():
            if "=" in part:
                k, v = part.split("=", 1)
                params[k] = v

        width = int(params.get("width", 40))
        height = int(params.get("height", 10))

        # Render and output
        frame_lines = render_frame(width, height)
        # Truncate to height (leave room for FRAME header = 1 line)
        max_lines = height - 1
        frame_lines = frame_lines[:max_lines]

        print(f"FRAME width={width} height={height}")
        for l in frame_lines:
            # Pad or truncate to width
            print(l[:width].ljust(width))
        sys.stdout.flush()


if __name__ == "__main__":
    main()
