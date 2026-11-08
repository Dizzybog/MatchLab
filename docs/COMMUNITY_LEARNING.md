# MatchLab Community Learning (GitHub only)

No additional server is required. Users explicitly opt in to creating up to
four public GitHub Issues per UTC day with suspected parser anomalies. GitHub
Actions validates **current official build SHA**, learning epoch and payload
schema and generates review data. Neither GitHub Issues nor its submitter
can be trusted as ground truth.

## Enable contributions

1. Pull the latest official `ghcr.io/dizzybog/matchlab:latest` image. It
   embeds the full Git SHA during the official build.
2. Create a [fine-grained GitHub personal access token](https://github.com/settings/personal-access-tokens/new):
   repository `Dizzybog/MatchLab` only; permission **Issues: Read and write**
   only. GitHub Issues must be enabled on the repository.
3. In your local, **uncommitted** Compose configuration:

   ~~~yaml
   environment:
     MATCHLAB_COMMUNITY_LEARNING_ENABLED: "true"
     MATCHLAB_COMMUNITY_GITHUB_TOKEN: ${MATCHLAB_COMMUNITY_GITHUB_TOKEN}
   ~~~

   Put the token in your own local `.env` or secret injector. Enabling the
   token **alone** must never trigger uploading. Restart your container.
4. Check `curl http://127.0.0.1:8765/api/community/status`. Both `enabled`
   and `ready` should be true. To stop, set the flag to false or revoke
   the token and restart. Do not share the token in GitHub, screenshots or chat.
5. The uploader reads only up to five bounded pages from the *current* local
   history every five minutes, and submits up to four suspected anomalies
   per UTC day. It never sends full debug traces, filesystem paths, tracker
   URLs, passwords or token contents. It rejects URLs, `@`, paths and obvious
   secrets; **basenames can still contain private identifying details**.
   All GitHub Issues are public and may be permanent. Only enable this
   if your torrent filenames are suitable for public release.
6. To contribute without a token, manually open an Issue with a
   **non-sensitive** example, current behavior, expected behavior and
   independent IMDb/AniDB references.

## Strict version eligibility, not trust

- Protocol `1` defines the report format, learning epoch `1` defines
  compatibility of labels and features.
- Official Docker images embed their Git SHA *at compilation*, rather than
  trusting a runtime version environment variable.
- The client requests GitHub's latest upstream `main` commit SHA; if the
  request fails, the SHA is unavailable, or it differs from the embedded
  hash, uploads are disabled (fail closed). A new build/protocol/epoch
  starts the cursor at **the newest local request**. No old backlog is sent.
- The GitHub Action independently rejects any report not matching the
  exact **current** main SHA, protocol, and epoch and deduplicates its
  output; stale reports are never fed to training.
- A GitHub token **does not attest which binary is running**. Contributors
  can forge body fields, including SHA. Thus reports are only *untrusted
  hypotheses*. They cannot trigger automatic code execution, new
  rules, ground-truth labels or production merges.
- Verified regressions are retained because facts can survive parser
  updates. On an incompatible epoch change, old labels must be
  **revalidated** before training; unverified staging is invalidated.
  We do not erase trustworthy data solely because a new Docker
  version was published.

## Repository maintainer steps

1. Merge this PR after CI is green; keep Actions and Issues enabled.
2. The updated Docker publishing workflow passes `MATCHLAB_BUILD_SHA`
   to the release build, so unofficial/older images cannot report.
3. In GitHub's Actions tab, open **Community learning review** and
   run it manually or allow the daily schedule.
4. The workflow parses GitHub Issues strictly as untrusted structured
   data, checks version and epoch, deduplicates and emits a limited
   review artifact. It **does not execute anything from Issue content**.
5. Reproduce a submission using the current MatchLab version, validate
   the expected behavior against independent metadata, and commit a
   confirmed example to `learning/verified.jsonl`. Never accept a
   submitter's label as proven without verification.
6. `scripts/community_learning.py` optionally trains a *shadow-mode*
   Bernoulli Naive Bayes review-priority model once there are sufficient
   verified positive and negative examples and a holdout test passes.
   The generated model is an Actions artifact, **not loaded into the
   production matcher**. This deliberately separates building a
   trustworthy dataset from activating automatic matching decisions.
7. If model/rule auto-updates are wanted later, add reviewed, versioned
   releases with rollback. Do not auto-merge unverified reports.

There is no persistent shared SQLite database on your server. Public
Issues are the submissions; bounded GitHub workflow artifacts are
temporary; `learning/verified.jsonl` is the reviewed compact dataset.
GitHub may rate-limit or disable scheduled workflows on inactive public
repositories. Monitor Actions status and use manual dispatch if needed.

