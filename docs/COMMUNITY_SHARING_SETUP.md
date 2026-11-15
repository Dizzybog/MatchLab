# Community sharing setup

Step-by-step setup for sharing locally learned regex proposals with the MatchLab project on GitHub. Local ML must be enabled first; see [Machine Learning overview](ML_OVERVIEW.md).

## Overview

GitHub sharing is **off by default**, even when local ML is on. MatchLab only posts proposed regex changes and selected diagnostics as comments on Issues in [Dizzybog/MatchLab](https://github.com/Dizzybog/MatchLab). It does **not** create Issues or merge PRs.

The trusted **Community Learning GitHub Action** now independently tests proposed
source-handler regex changes against the original Rust parser, known-good
torrent titles and maintainer-verified expected fields. It flags redundant and
regressive rules; only independently proven extensions can stage an exact
source-regex union and new Rust regression tests in a **human-review PR**. 
Neither an ML score nor an Issue comment can directly edit the parser or
automatically merge a PR. See [the automated regex impact reviewer](COMMUNITY_LEARNING.md#automatic-native-regex-impact-review).

## 1. Create the right GitHub token

**If you own the upstream MatchLab repository** (or it is selectable in your GitHub token settings), use a **fine-grained personal access token**:

1. Log into GitHub and open **Settings → Developer settings → Personal access tokens → Fine-grained tokens → Generate new token**. [Open GitHub's token settings](https://github.com/settings/personal-access-tokens/new).
2. Name it `MatchLab community sharing` and set a limited expiration (for example 90 days).
3. Set **Resource owner** to the account that owns the upstream repository, **Dizzybog**. Under **Repository access**, choose **Only select repositories** and select **MatchLab**.
4. Under **Repository permissions**, set **Issues → Read and write**. **Metadata → Read-only** is automatic. No **Contents**, **Actions**, **Administration** or **Pull requests** write access is needed.
5. Select **Generate token**, then copy the secret immediately. GitHub does not show its full value again.

**If you are an external contributor and cannot select `Dizzybog/MatchLab`**, GitHub currently cannot use a fine-grained PAT to contribute to public repositories outside your account/organization. Use **Settings → Developer settings → Personal access tokens → Tokens (classic) → Generate new token (classic)** instead ([open classic token settings](https://github.com/settings/tokens/new)). Select **`public_repo` only**, set a short expiration and generate the token. **Security tradeoff:** `public_repo` is broader than single-repository access and can authorize actions across public repositories, so treat this token as sensitive and revoke it when you stop sharing. **Do not enable `repo`** unless you separately need private-repository access.

References: [GitHub's PAT guide](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens) and [Issue-comment API permissions](https://docs.github.com/en/rest/issues/comments#create-an-issue-comment).

## 2. Keep the token out of Compose and Git

Create a `.env` file **next to your `compose.yaml`**:

```dotenv
MATCHLAB_COMMUNITY_GITHUB_TOKEN=github_pat_REPLACE_WITH_YOUR_TOKEN
```

For a classic token the prefix will typically be `ghp_` instead. Restrict file access on Linux:

```bash
chmod 600 .env
```

Add these environment variables to the **existing** `matchlab` service, keeping the `/data` volume persistent:

```yaml
services:
  matchlab:
    environment:
      MATCHLAB_ML_SIZE_GB: "10"
      MATCHLAB_COMMUNITY_LEARNING_ENABLED: "true"
      MATCHLAB_COMMUNITY_GITHUB_TOKEN: "${MATCHLAB_COMMUNITY_GITHUB_TOKEN:?Set the token in .env}"
```

This is an **environment excerpt**, not a replacement for your whole Compose file. Ensure `.env` is in `.gitignore`, never commit it, and avoid publishing `docker compose config` output because it may contain expanded secrets.

## 3. Restart and verify token authentication

```bash
docker compose up -d
docker compose logs --tail=100 matchlab
curl -s http://127.0.0.1:8765/api/community/status
```

On startup, when GitHub accepts the token and the upstream repository's Issues are accessible, expect an **INFO** entry similar to:

```text
community sharing GitHub token valid: authenticated and upstream Issues reachable; comment permission verified on first successful upload
```

The status API returns `token_authentication: "authenticated"` and `ready: true` after successful verification. An invalid/revoked token yields `rejected`; an API/network outage yields `network_unavailable`. MatchLab retries verification without stopping parsing or local ML.

**Important:** Startup verification is read-only. It verifies the credential and target repository, **not** the right to POST an Issue comment, since testing that would publicly publish a comment. Actual comment permission is confirmed only on the first successful GitHub submission (HTTP 201). If comments fail with HTTP 403, recheck **Issues → Read and write** (fine-grained) or **`public_repo`** (classic), plus repository interaction restrictions.

MatchLab archives learned regex identities **even before GitHub sharing is enabled** in the persistent ML database. Turning sharing on later drains earlier and current rules in bounded daily batches. Older, retired or under-threshold patterns remain **review-only**, never auto-activated. Confirmed GitHub deliveries are deduplicated locally and by GitHub's global ledger; failures remain retryable. The GitHub workflow also retains a durable queue for proposals not yet reviewed. **Nothing is merged automatically.** Technical/TV/source-regex suggestions first enter a non-executable review queue; independently verified rules must be integrated before other users receive them. Reports can include sanitized release filenames, so only enable sharing if you accept publishing that information.

See [Machine Learning and Community Learning](COMMUNITY_LEARNING.md) for supported patterns, limitations, privacy considerations, validation rules and contributor setup.
