# gpg-cmdash-wrapper

A reproducible, version-controlled wrapper that lets `git` sign commits
on hosts where the system's `gpg-agent` cannot satisfy the passphrase
request through its standard cache path (e.g. `ERR 67108933 Not implemented`
on the `preset_passphrase` assuan command, or `gpg-preset-passphrase`
binary is not installed, or the host has no controlling TTY).

## Why this exists

The host's `gpg-agent 2.4.9` returns `ERR 67108933 Not implemented` on
the `preset_passphrase` assuan command, even with `allow-preset-passphrase`
in `~/.gnupg/gpg-agent.conf`. The `gpg-preset-passphrase` binary is
also not installed. Without a working cache path, `git commit -S`
fails because the agent cannot deliver the passphrase to gpg.

This wrapper bypasses the agent-cache path: it reads the user's GPG
key passphrase from a host-local file and feeds it to `gpg` directly
via `--pinentry-mode loopback --no-tty --batch --passphrase-fd 3`.
Every commit invocation re-enters the passphrase, but the wrapper
itself is committed and reproducible across hosts.

See `docs/ci-evidence.md` `### Audit cycle 12 - reproducible GPG signing wrapper`
for the full diagnostic + trade-off analysis.

## Setup (host-local; not committed)

The wrapper is committed to the public repo, so it contains NO secrets.
The passphrase lives in a host-local file (chmod 600, NOT in the repo).

### Step 1: Wire git's `gpg.program` to the wrapper

```bash
# Local-repo only (not global).
git config --local gpg.program "$(pwd)/scripts/gpg-cmdash-wrapper.sh"

# Re-enable auto-signing for future commits.
git config --local commit.gpgsign true
```

Or use the `just gpg-setup` recipe:

```bash
just gpg-setup
```

The wrapper script needs to be executable on disk:

```bash
chmod 700 scripts/gpg-cmdash-wrapper.sh
```

### Step 2: Create the host-local passphrase file

```bash
mkdir -p ~/.config/cmdash
printf '%s' 'YOUR_PASSPHRASE_HERE' > ~/.config/cmdash/gpg-passphrase
chmod 600 ~/.config/cmdash/gpg-passphrase
```

`printf '%s'` is used (not `echo`) to ensure NO trailing newline is
written. Some `gpg` builds reject trailing newlines; `printf '%s'`
is the safe write.

### Step 3: Verify

```bash
# Throwaway probe: should sign cleanly via the wrapper.
git commit --allow-empty -m 'verify: gpg wrapper sanity' -S
git log -1 --show-signature --pretty=fuller
git reset --hard HEAD~1
```

If `git verify-commit HEAD` (after the probe + reset) returns
`gpg: Good signature from <uid>`, the wrapper is wired correctly.

## Customization

Override the passphrase file location by setting the
`CMDASH_GPG_PASSPHRASE_FILE` environment variable before invoking
`git commit`:

```bash
CMDASH_GPG_PASSPHRASE_FILE=/path/to/my/secret \
    git commit -m '...'
```

This is useful for CI environments where the secret is mounted at
a different path than the dev-host default.

## Security

- The wrapper is committed to the public repo; it contains NO secrets.
- The passphrase lives in `$PASSPHRASE_FILE` (default
  `~/.config/cmdash/gpg-passphrase`, chmod 600, host-local, NOT
  committed). The `.gitignore` excludes any `*gpg-passphrase*` files
  to prevent accidental commits.
- The passphrase is fed to `gpg` via file descriptor 3
  (`--passphrase-fd 3`), which protects it from `ps` command-line
  snooping and frees `stdin` (`FD 0`) for git's payload.
- `--pinentry-mode loopback` ensures `gpg` does not try to open
  `/dev/tty` for a pinentry prompt (which would fail in non-TTY
  contexts like this basher session).
- The trade-off: the passphrase file is persistent on disk in
  cleartext (chmod 600, but a stolen laptop can read it). The
  standard gpg-agent cache has the same property, but only after
  the user has entered the passphrase once at a TTY.

## Removal

To fall back to standard gpg-agent signing (or the
`--no-gpgsign` per-command workaround):

```bash
# Stop using the wrapper.
git config --local --unset gpg.program
git config --local commit.gpgsign false   # or `true` if a working agent is set up

# Secure-delete the passphrase file.
shred -u ~/.config/cmdash/gpg-passphrase
```

The `scripts/gpg-cmdash-wrapper.sh` and this README stay in the
repo (they're harmless) but become inert once `gpg.program` is
unset.
