#!/usr/bin/env python3
"""Verify that docs/configuration.md section numbering is consistent.

Top-level headings (## N. Title) must increment sequentially starting at 1.
Sub-level headings (### N.M. Title) must match the current top-level section
and increment sequentially starting at 1 within each top-level section.
"""

import re
import sys


def check_numbering(filepath: str) -> None:
    top_expected = 1
    current_top = 0
    sub_expected = 1

    with open(filepath, encoding="utf-8") as f:
        for line_num, line in enumerate(f, 1):
            top_match = re.match(r"^## (\d+)\.", line)
            if top_match:
                val = int(top_match.group(1))
                if val != top_expected:
                    print(
                        f"{filepath}:{line_num}: expected `## {top_expected}.`, "
                        f"got `## {val}.`",
                        file=sys.stderr,
                    )
                    sys.exit(1)
                current_top = val
                top_expected += 1
                sub_expected = 1
                continue

            sub_match = re.match(r"^### (\d+)\.(\d+)\.", line)
            if sub_match:
                top_val = int(sub_match.group(1))
                sub_val = int(sub_match.group(2))
                if top_val != current_top or sub_val != sub_expected:
                    print(
                        f"{filepath}:{line_num}: expected "
                        f"`### {current_top}.{sub_expected}.`, "
                        f"got `### {top_val}.{sub_val}.`",
                        file=sys.stderr,
                    )
                    sys.exit(1)
                sub_expected += 1

    print(f"PASS: {filepath} numbering is consistent.")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <markdown-file>", file=sys.stderr)
        sys.exit(2)
    check_numbering(sys.argv[1])
