#!/bin/sh
# Git status widget (interval mode). Shows the current branch and a short
# status summary, tagged so `parse_tags` styles it.
cd "${CMDASH_GIT_DIR:-.}" 2>/dev/null || exit 0
branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null) || exit 0
echo "[info] $branch"
git status --short 2>/dev/null | head -n 20
