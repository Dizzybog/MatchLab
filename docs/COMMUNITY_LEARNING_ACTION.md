# Community Learning Review: durable operation

The daily `.github/workflows/community-learning.yml` workflow runs on trusted
`main` (scheduled or manually dispatched). A self-hosted MatchLab can opt in
to posting **unverified** parser observations and proposed regex changes as
comments on the daily `[community-learning] YYYY-MM-DD` GitHub issue.

## Pipeline

1. Find the official `github-actions[bot]` issues through **all** paginated
   repository issues, including today's issue and older threads updated in the
   last 60 days. Create one current daily issue if missing. Recover recent
   comments even if the Action did not run the previous day. Never execute
   issue/comment content.
2. Restore the newest **verified**, nonzero GitHub Release snapshot for each
   state family: `community-learning-ledger` (fingerprints) and
   `community-learning-backlog` (unreviewed proposals), and
   `community-learning-decisions` (source-versioned decisions).
3. Parse historical anomaly reports with the current Rust parser; discard
   problems already resolved; verify rule schemas, source-commit ancestry and
   bounded safe grammar. A user's assertion of a correct IMDb ID, AniDB ID, or
   field is never independent ground truth.
4. Merge new proposals into the durable queue. Reuse audit decisions only
   if the immutable native handler catalogue **and** independently labeled
   fixture corpus have the identical SHA-256 signature. Unchanged `no_effect`,
   `regression`, `invalid_source`, `review_only`, and `needs_independent_evidence`
   cases stay in the queue but do not waste CPU on repeated reviews.
   Any source or trusted-fixture edit automatically makes them reviewable again.
   Write and **verify** an immutable
   `backlog-RUN-ATTEMPT-prepared.json` Release asset *before screening*.
5. Choose up to 24 proposals for this run, screen at most one candidate for
   human review. Associate rules with up to 12 real torrent titles whose
   technical markers occur in the **same validated comment batch**; keep the
   title file runner-local and never attach it to the public PR.
6. Build bounded plans from **all validated regex families**. Proposals
   for technical fields (quality, codec, audio, languages, extension, etc.)
   reuse an exact native handler with the required output transform. TV rules
   plan season and episode handlers **together**. Corrective TV and general
   handler-specific rules reuse the current source identity. Source patches
   retain their exact original handler. Ambiguous transforms, historical
   proposals, and rules with outdated source indexes are explicitly reported
   as review-only rather than guessed or executed.
7. The trusted **Rust** auditor executes each planned patch, addition or
   corrective rule with MatchLab's full native parser. It compares the current
   parser to the candidate on maintainer-labeled
   `learning/regex_review_fixtures.json`, protected negatives, and actual
   community torrent titles. Community title changes are **telemetry**, not
   proof: user-supplied parsing/IMDb labels are not a trusted oracle.
8. A native source change is allowed only when at least **two distinct,
   independently labeled titles** improve, **every modified field** has a
   verified improvement, and the full protected and unrelated-field regression
   counts are zero. The source writer re-derives and checks the exact plan,
   then clones native handlers with original priority/flags/options/transform,
   or replaces only the anchored original regex. Proven fixtures are promoted
   to `learning/regex_promoted_tests.json`. Unproven suggestions remain
   diagnostics in the durable backlog.
9. Full Rust regression tests/checks run before opening a human-review PR.
   The bot creates a **unique per-run branch**, never merges its own PR, and
   explicitly dispatches Rust CI because `GITHUB_TOKEN`-authored PRs do not
   trigger ordinary `pull_request` workflows. No PR is opened for JSON-only,
   no-effect or formatting-only changes.
10. Mark a fingerprint reviewed only after its PR was actually created.
   Persist and verify a new finalized backlog snapshot and dedup-ledger
   snapshot, plus a separate verified versioned review-decision snapshot.
   Publish bounded audit artifacts (no private issue titles).
11. **Only after durable state and artifacts are saved**, close old bot-owned
    idle daily issues and unreviewed stale bot PRs, then delete old bot-owned
    review branches after a recovery grace period. Leave human PRs, non-bot
    issues and anything with the `community-keep-open` label untouched.

## Recovery and limits

- **Fresh repository / existing empty Release:** bootstrap a logically empty
  ledger and backlog. For an empty fingerprint list, upload a nonzero file
  containing `# matchlab-community-ledger-v1`, never a zero-byte upload.
- **Old Release:** `fingerprints.txt` or `backlog.json` loads normally.
  Missing review decisions bootstrap to an empty map without disturbing the
  existing backlog and dedup ledger.
- **Upload failure / partial run:** the previous immutable asset remains
  available. Confirm the replacement exists with nonzero size **before**
  pruning previous snapshots. Retain six most recent snapshots per state type.
- **Malformed latest stored state:** stop; do not silently reset state,
  discard dedup hashes or create a PR.
- **Too many issue comments, rules, stale snapshots or pending proposals:**
  stop with an explicit error rather than silently truncate the data.
- **A PR is already waiting for human review:** do not duplicate it; rotate
  other unprocessed proposals in the durable backlog. Retrying is safe.
- **Deferred review:** a cached decision does not delete, deduplicate or
  acknowledge a proposal as merged. A new native handler catalogue or newly
  verified fixture set invalidates its cache and triggers re-evaluation.
- **Unverified suggestion / no source handler identity:** report review-only
  diagnostic information and retain it in the backlog. This is not grounds
  for a code-changing PR. Safe source-handler automation is deliberately
  narrower than the set of patterns MatchLab can *suggest*.
- **Cleanup:** normal bot-owned daily issues need 21 days of age and 7 days
  idle; late-comment threads get extra protection, with an archival fallback
  at 90 days old and 60 days idle. Bot review PRs require 30 days idle and
  no human review. Branch deletion has separate grace periods and rechecks
  live refs immediately before deletion. Closing never deletes discussion
  comments or unreviewed report history.

The workflow does not require an external AI service, a third-party API key,
or access to a live MatchLab container. The only mutable action on the native
parser is a test-backed **PR**, not a production change.

## Local checks

```bash
python3 -m unittest discover -s scripts/tests -v
RUSTFLAGS="-D warnings" cargo test --workspace --locked
RUSTFLAGS="-D warnings" cargo check --workspace --all-targets --locked
```
