# Community Learning: GitHub-only, opt-in, and version-aware

No central Learning Hub service or shared SQLite instance is required.
GitHub Issues hold opt-in **untrusted anomaly reports**, GitHub Actions
replays and reviews them, and a GitHub Release stores the deduplication ledger.
No public report may modify production matches, train unsupervised corrections
or trigger an automatic merge.

## How to contribute

1. Install an official published MatchLab Docker image.
2. Enable repository **Issues** and **Actions** (the maintainer does this).
   The workflow creates one daily Issue named
   `[community-learning] YYYY-MM-DD`, in UTC. It may not appear until the
   daily workflow has run.
3. Create a GitHub personal access token for *your own account*:
   - **Repository owners / members:** Use a
     [fine-grained PAT](https://github.com/settings/personal-access-tokens/new)
     restricted to `Dizzybog/MatchLab`, with **Issues: Read and write**.
   - **External contributors:** GitHub currently does **not** support
     fine-grained PAT write access to a public repository you do not belong
     to. A [classic PAT](https://github.com/settings/tokens) with
     `public_repo` may be used instead, but this permission covers other
     public repositories accessible by your account and is broader.
     **Understand the risk; use a dedicated low-privilege account if
     appropriate, or submit a manual Issue instead.**
   Tokens belong only on your own host. Never put them in issues or chat.
4. In your **uncommitted** local `.env` file add the token. In Compose:

   ~~~yaml
   environment:
     MATCHLAB_COMMUNITY_LEARNING_ENABLED: "true"
     MATCHLAB_COMMUNITY_GITHUB_TOKEN: ${MATCHLAB_COMMUNITY_GITHUB_TOKEN}
   ~~~

   Opt-in is **disabled by default**. Providing a token alone does not
   enable uploads. Restart MatchLab.
5. Check `curl http://127.0.0.1:8765/api/community/status`. It should
   report `enabled: true` and `ready: true`. You can opt out at any time:
   disable the flag, redeploy, and revoke your token.
6. MatchLab scans the last 1,000 local history entries incrementally every
   five minutes, adds **up to 16 distinct suspicious examples** to a
   persistent pending SQLite table, and waits until **24 hours have elapsed**.
   It then posts at most **one batch comment** to today's shared daily
   GitHub Issue, **only if that Issue was created by `github-actions[bot]`**.
   This prevents malicious users from impersonating a daily thread by title.
   It does not create its own Issues. Failed checks are retried
   later; after an attempted POST a batch is marked as sent *before* the
   network call, so an uncertain timeout cannot cause repeated comments.
   This trades possible lost submissions for safety against GitHub spam.
7. A SHA-256 fingerprint calculated from the normalized release filename
   plus anomaly codes is stored persistently after a submission attempt.
   Repeated observations in future 24-hour windows **and after Docker
   upgrades** do not produce another report on that installation. GitHub
   Actions also deduplicates across contributors and days using a small
   fingerprint ledger kept in a GitHub Release asset.
8. Shared Issue comments can expose torrent **basenames**. Although obvious
   paths, URLs, email addresses and credentials are filtered, this does not
   guarantee that basenames are private. Do not enable sharing for private
   or identifying releases. Raw debug traces, full paths, trackers and
   authorization tokens are never included.

## Accept older releases, but never learn outdated errors

**A report from an older MatchLab version is welcome if the bug still
exists in current code.** An older client need not have the newest image
to contribute, provided its embedded source SHA is on the upstream `main`
history and its **report protocol and learning epoch** are compatible.

The safety boundary is the GitHub Action, not the reported version:
- Verify the submitted source SHA is an ancestor of current `main` with a
  complete Git history. Unrelated, malformed or non-mainline source commits
  are excluded.
- Parse each candidate title again using the **current Rust parser** compiled
  by the workflow from main. If an explicitly marked season/episode
  disagreement is **demonstrably fixed**, discard that candidate.
- For matching or AniDB/IMDb crosswalk problems, simple parsing alone is
  **not enough** to establish that the bug is still present or fixed.
  Mark these cases `needs_current_metadata_review` for independent
  reproduction on current metadata. Do not claim an unverified match
  was corrected.
- Only independently verified, still relevant examples enter the versioned
  `learning/verified.jsonl` dataset (with current SHA validation). Human
  reviewers must confirm and update the evidence and labels as necessary.
- Older unverified pending data is discarded after any protocol, learning
  epoch or executable change; the local reporting cursor moves to newest
  history. **The already-reported fingerprint registry survives upgrades**
  to avoid resubmission. Verified historical regression cases are retained,
  but must be revalidated for incompatible epochs before training.

**Build hashes are not an attestation:** anyone with permission to comment
can forge GitHub fields. Version checking stops accidental stale/unsupported
reports, not malicious impersonation. Reports are hypotheses, not truth.

## GitHub maintainer setup

1. Review and merge the implementation PR after successful Rust, Python,
   Docker and security CI. Confirm **Issues**, **Actions** and releases
   are enabled. Do not enable community uploading before the PR is merged.
2. GitHub's **Publish Docker image** workflow embeds
   `MATCHLAB_BUILD_SHA` in the executable at compile time. Source SHA
   cannot be changed via a runtime environment variable.
3. The trusted `Community learning review` workflow runs daily at about
   00:23 UTC or by manual dispatch. It creates/reuses **at most one
   dated GitHub Issue** for all installations, and reads the previous
   day's batch comments. No Issue text is ever executed as a command.
4. The workflow downloads the persistent SHA-256 fingerprint ledger from
   release `community-learning-ledger`, validates all reports, runs the
   current Rust parser, excludes resolved parser errors and previously
   seen fingerprints, and updates that ledger. A ledger overflow or
   malformed ledger stops processing rather than dropping dedup history.
   Review artifacts are stored for 7 days.
5. Review pending identity-related reports with **current IMDb/AniDB
   mappings**. Reproduce the failure; create or update a regression test
   and commit independently confirmed examples to
   `learning/verified.jsonl`.
6. The shadow-mode Bernoulli Naive Bayes model is trained only with
   enough verified **positive and negative** examples validated on the
   current source SHA. It uses a fixed holdout split and explicit quality
   thresholds. The model artifact does **not** change runtime matching.
7. When model accuracy and dataset diversity are established, add a
   separately reviewed signed-release and rollback workflow for
   production rule/model updates. This PR does not enable automatic
   changes of SID/ASID, self-training from unverified guesses or
   unattended merging.

## Resource boundaries

- Clients: up to 16 unique reports per 24 hours; one comment every 24 hours
  at most; a bounded pending queue and a local database of at most 50,000
  reported fingerprints. The uploader will stop if the local registry
  reaches that limit, rather than losing dedup history. Reports are never
  sent unless the user explicitly enables them.
- GitHub: at most one shared Issue each UTC day; bounded ingestion of up to
  500 comments and 200 unique candidates per daily run; a maximum of
  100,000 global fingerprint entries; 7-day review artifact retention.
- GitHub Actions uses read/write Issues to create the shared thread,
  read/write Contents to maintain the GitHub Release dedup asset,
  and *no third-party automation rights to merge PRs*. Only trusted
  default-branch workflows perform these operations.
- One-comment-per-installation does not eliminate GitHub's abuse/rate
  limits. For very large adoption an alternative transport will eventually
  be necessary; the system fails closed rather than flooding GitHub.

This design follows the useful distinction made by active-learning and
label-quality projects: **unverified signals are acquisition candidates,
not training labels**. Learning quality comes from independently
confirmed labels and evaluation on current data, not merely from having
many contributors.
