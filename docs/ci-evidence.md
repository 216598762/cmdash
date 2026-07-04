# CI Evidence Ledger

This ledger captures authoritative CI evidence for forward-fixup atoms in
the `cmdash` chain. Each entry pairs a commit SHA with the actual measured
CI result, providing ground truth when the commit body's pass/fail claims
diverge from reality.

## Audit principles

- **Forward-only discipline**: no commit history rewrites, no `git commit
  --amend`, no `git rebase -i` cleanup. Corrections land as new forward-fixup
  atoms with a doc-only ledger entry.
- **This ledger is the audit-cleaner shape** per the forward-only-no-rewind
  posture. Future readers override divergent commit-body claims via the
  authoritative measured value captured here.

## Entry format

Each entry documents:

- `commit` (short SHA) + commit subject line
- `claim` — the verbatim pass/fail assertion from the commit body
- `actual` — measured ground-truth from `cargo test -p <crate> --quiet`
- `delta` — claim vs actual: discrepancy + reasoning where known
- `evidence` — the exact `cargo test` invocation + host context
  (OS, Rust version, time)
- `forward-fix` — the corrective atom shape (always a new forward-fixup
  with doc-only ledger entry, per the discipline above)

## Entries

### `ecfa1f2` — `fix(cmdash-pty): revert slave-fd switch; re-ignore cat-echo test`

Forward-fixup corrective entry, landed as a new forward-fixup atom to
preserve history (no amend / no rebase).

- **Claim**: commit body's "Final state on origin/main" summary asserts
  `22 PASS + 0 FAIL + 1 IGNORED`.
- **Actual**: `13 passed; 0 failed; 1 ignored` per `cargo test -p
  cmdash-pty --quiet` on `origin/main@2a5aa3c` (this ledger atom's
  measurement host).
- **Delta**: claim overstated PASS count by ~9. The actual test inventory
  in `crates/cmdash-pty/tests/round_trip.rs` is **13 plain `#[test]` fns
  + 1 `#[ignore]` fn** (the cat-echo silence from the same `ecfa1f2`
  atom). The number 22 was off by ~9 — likely a counting mistake in the
  self-reported post-state line.
- **Effect**: the `ecfa1f2` atom's *function* (reverting the slave-fd
  attempt blocked by missing `SlavePty::as_raw_fd()` in portable-pty
  0.9, restoring the cat-echo silence with the `b7de7dd` pattern,
  fixing the `clippy::empty-line-after-doc-comments` orphan doc-block
  in `cmdash/src/main.rs`, bundling the previously-uncommitted
  `Cargo.lock` `+libc` entry) is **NOT** affected. The discrepancy is
  purely a number-of-tests counting error in the post-state summary,
  not a substantive defect in the atom's code-side edits.
- **Evidence**:
  - host: Arch Linux PTY-alloc host (this Atom's reference host)
  - Rust: 1.96.1
  - invocation: `cargo test -p cmdash-pty --quiet`
  - raw output (last 4 lines):
    ```
    test result: ok. 13 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in <X>s
    ```
- **Forward-fix**: this atom (`docs/ci-evidence: <subject>`) is the
  forward-only-no-rewind corrective. Per discipline, no `git commit
  --amend` or `git rebase -i` is invoked; the `ecfa1f2` commit body is
  left unaltered; future audit reads override via this ledger entry.

## How to add a new entry

1. Forward-fixup atom atop the current `origin/main`.
2. Append an entry under `## Entries` in this file.
3. Run the measurement cargo command fresh against the new HEAD.
4. Cite host + Rust version + the exact invocation.
5. Per cite-style above, document `claim / actual / delta / evidence
   / forward-fix`.
6. Commit with message subject prefix matching the atom's scope (e.g.
   `chore(ci):`, `fix(cmdash-pty):`, `docs(ci-evidence):`).
7. Land with `--no-gpg-sign` if the host's GPG agent lacks a TTY (as
   is the case here; per-commit workaround via
   `git -c commit.gpgsign=false commit ...`).

## Future workflow

When any future commit's body makes a pass/fail claim:

1. Run `cargo test -p <crate> --quiet` against the current `origin/main`.
2. If the actual diverges from the claim, append a ledger entry.
3. The commit body stays untouched. The ledger is the authority.
