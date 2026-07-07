#!/usr/bin/env bash
# gpg-cmdash-wrapper.sh -- reproducible GPG signing wrapper for cmdash.
#
# Git invokes this script as its `gpg.program`. The wrapper:
#  - reads the user's GPG key passphrase from a host-local file (default
#    ~/.config/cmdash/gpg-passphrase; override via CMDASH_GPG_PASSPHRASE_FILE
#    env var);
#  - exec's gpg with `--pinentry-mode loopback --no-tty --batch
#    --passphrase-fd 3`, feeding the passphrase on fd 3.
#
# This exists because the host's gpg-agent (gpg-agent 2.4.9) returns
# `ERR 67108933 Not implemented` on the `preset_passphrase` assuan
# command (despite `allow-preset-passphrase` being in
# ~/.gnupg/gpg-agent.conf), and the `gpg-preset-passphrase` binary is
# not installed in this host's PATH. The wrapper bypasses the
# agent-cache path so commits can sign without manual `--no-gpgsign`
# per-command workarounds.
#
# See docs/ci-evidence.md `### Audit cycle 12 - reproducible GPG signing wrapper`
# for the full diagnostic + trade-off analysis.
#
# SECURITY: this script is COMMITTED to the public repo and contains
# NO secrets. The passphrase lives in $PASSPHRASE_FILE (host-local,
# chmod 600, NOT in the repo).

set -euo pipefail

PASSPHRASE_FILE="${CMDASH_GPG_PASSPHRASE_FILE:-$HOME/.config/cmdash/gpg-passphrase}"

if [ ! -f "$PASSPHRASE_FILE" ]; then
    echo "::ERROR:: gpg-cmdash-wrapper: passphrase file missing at '$PASSPHRASE_FILE'." >&2
    echo "::ERROR:: Setup hint:" >&2
    echo "::ERROR::   mkdir -p \"$(dirname "$PASSPHRASE_FILE")\"" >&2
    echo "::ERROR::   printf '%s' 'YOUR_PASSPHRASE_HERE' > \"$PASSPHRASE_FILE\"" >&2
    echo "::ERROR::   chmod 600 \"$PASSPHRASE_FILE\"" >&2
    echo "::ERROR::   (Do NOT commit the passphrase file. Keep it host-local.)" >&2
    exit 1
fi

# Belt-and-suspenders perms check (warn-only, not fail-fast). The
# README documents `chmod 600` as a setup step, but a runtime hint
# catches the case where the user skips the README and the file is
# world-readable. The check is portable: GNU `stat -c %a` on Linux,
# BSD `stat -f %Lp` on macOS; either way we coerce the perms to a
# 3-digit octal string and warn on anything other than 600/400.
if command -v stat >/dev/null 2>&1; then
    if stat -c %a "$PASSPHRASE_FILE" >/dev/null 2>&1; then
        PERMS=$(stat -c %a "$PASSPHRASE_FILE")
    else
        PERMS=$(stat -f %Lp "$PASSPHRASE_FILE")
    fi
    if [ -n "${PERMS:-}" ] && [ "$PERMS" != "600" ] && [ "$PERMS" != "400" ]; then
        echo "::WARN:: gpg-cmdash-wrapper: passphrase file perms=$PERMS (expected 600); 'chmod 600 \"$PASSPHRASE_FILE\"' is recommended." >&2
    fi
fi

# Forward all args to gpg. The wrapper exists only to inject the
# loopback-pinentry + no-tty + batch flags + the passphrase fd 3
# attachment. The process substitution `<( head ... | tr ... )`
# strips trailing newlines from the passphrase file so the wrapper
# is robust to files written via `echo "pass" > file` AND
# `printf '%s' "pass" > file` (the README recommends `printf`
# because some gpg builds are intolerant of trailing-newline bytes
# in the passphrase; this is the belt-and-suspenders code-level
# safety). gpg's --passphrase-fd 3 reads from the fd 3 attached
# pipe; loopback pinentry suppresses any /dev/tty prompts.
exec 3< <(head -c 4096 "$PASSPHRASE_FILE" | tr -d '\n')
exec gpg --pinentry-mode loopback --no-tty --batch --passphrase-fd 3 "$@"
