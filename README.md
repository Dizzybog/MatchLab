# MatchLab

**MatchLab** is a self-hosted API that identifies movies, TV shows and anime from torrent release names. It extracts the title and episode information, finds the corresponding IMDb title and, where verified, maps anime episodes to AniDB.

For example, `Breaking.Bad.S02E03.1080p.WEB-DL` contains a series title, season 2, episode 3 and video-quality tags. MatchLab parses those parts and searches its local metadata index. **It does not find or download torrents.**

MatchLab is written in Rust. It uses **Tantivy** to search titles and **SQLite** for metadata. It provides an HTTP API and a web dashboard.

## Get started

Install Docker with Docker Compose. Create a `compose.yaml` file:

```yaml
services:
  matchlab:
    image: ghcr.io/dizzybog/matchlab:latest
    container_name: matchlab
    restart: unless-stopped
    cpus: 2.0
    mem_limit: 6g
    pids_limit: 256
    environment:
      MATCHLAB_DB: /data/metadata.sqlite3
      MATCHLAB_HOST: 0.0.0.0:8765
      MATCHLAB_MAX_CONCURRENT_MATCHES: "2"
    ports:
      - "127.0.0.1:8765:8765"
    volumes:
      - metadata:/data

volumes:
  metadata:
```

Start the service:

```bash
docker compose up -d
docker compose logs -f matchlab
curl http://127.0.0.1:8765/health
```

The first run downloads public IMDb and anime metadata and builds the local index. This needs an internet connection, disk space and time. `/health` returns **503** until the index is ready and **200** when matching is available. Later metadata refreshes normally run every seven days. The dashboard is at `http://127.0.0.1:8765/`.

The example is limited to two CPUs and 6 GB RAM; these are starting limits, not guaranteed minimum requirements. To update, run `docker compose pull && docker compose up -d`. A full configuration is available in [compose.yaml](compose.yaml).

## Use the API

Match a release name:

```bash
curl -X POST http://127.0.0.1:8765/match \
  -H 'Content-Type: application/json' \
  -d '{"title":"Supernatural.S01E01-E22.1080p"}'
```

Example response:

```json
{
  "sid_start": "tt0460681:1:1",
  "sid_end": "tt0460681:1:22",
  "asid_start": null,
  "asid_end": null
}
```

A **SID** is an IMDb title ID, optionally followed by season and episode. An **ASID** identifies an AniDB anime episode; it is returned only when the mapping can be verified. Unmatched values are `null`.

| Endpoint | Purpose |
| --- | --- |
| `GET/POST /match` | Match a release to IMDb and optional AniDB episodes |
| `GET/POST /parse` | Extract release fields, including normalized ISO `date`, without matching |
| `GET/POST /alias` | Look up alternate titles and related IDs |
| `GET/POST /episode/resolve` | Convert verified IMDb and AniDB episode coordinates |
| `POST /debug/match` | Inspect matching with the same strict checks as `/match` |
| `POST /api/dashboard/diagnose` | Explore title-only matches without production API eligibility checks |
| `GET /health` | Check whether matching is ready |
| `GET /` | Open the dashboard |

The public `/match` and `/debug/match` endpoints have identical strict matching requirements. The dashboard's diagnostic endpoint is intentionally more permissive. If metadata is missing or conflicting, MatchLab avoids inventing a match. See [API documentation](docs/API.md) for request options, full responses and other endpoints.

## Optional Machine Learning

**MatchLab works normally without Machine Learning (ML).** When enabled, a small Rust model learns patterns from release names in the background. It can add new regex patterns or suggest replacements for existing ones. **ML never sets or overwrites parsed fields.** MatchLab's own parser interprets the compiled rules, and its matcher verifies IMDb/AniDB identities as usual. The model is not run on each request.

The learning process is:

1. Examine new release names for **four error types**: missing values, excessive extraction, incomplete extraction, and values assigned to the wrong fields (for example, swapped seasons and episodes). These are learning signals, not automatic corrections.
2. Group recurring patterns and train a small **FTRL-Proximal** model.
3. Test proposed regex rules against known values, conflicting patterns and negative examples.
4. Activate eligible rules in memory without restarting. More uncertain changes are kept for review.

The generated parser contains **458 ordered handlers across 48 field families**
(449 regex handlers and nine Rust functions). The new *field-general*
learner can derive safe regex candidates from existing native handler
provenance and meaning-preserving release-tag variants. It must prove
**incremental improvement**, preserve complete parser results, reject
duplicate/no-op rules and revalidate stored evidence after upgrades.
More ambiguous changes remain review-only until independent references
establish a reliable result. See the [complete parser and ML regex audit](docs/ML_REGEX_ENGINE.md)
for field families, transform safety, title-removal interactions, rule
lifecycle and remaining limitations.

**ML learns from every field that MatchLab parses**, including titles, years, seasons, episodes, languages, video and audio details, release groups, and other metadata. It looks for recurring naming patterns and can propose **new regex rules or improvements to existing ones** for any field. A rule is activated in the live parser **without restarting** only when sufficient independent evidence and field-specific safety checks support it; otherwise it remains inactive for further validation or review. Anime episode patterns still require verified metadata mappings. **ML changes regex only: MatchLab performs all field extraction and IMDb/AniDB matching. The existing `date` field participates in the native ML handler catalog, so new date syntaxes can be observed and proposed; unlike fixed technical labels, date changes require calendar and full-parser verification before they can become live rules.** With community sharing enabled, the exact regex additions and replacements can be proposed on GitHub. An ML score alone never activates a rule.

Enable learning with **one ML environment variable**:

```yaml
environment:
  MATCHLAB_ML_SIZE_GB: "10"
```

`0` (the default) disables local ML; `1` to `128` enables it with that
many GiB reserved as a **best-effort total ML budget**, not an operating-system
filesystem quota. MatchLab splits this into **50% training evidence, 30% result
cache and 20% observation queue/history**. A 10 GiB budget derives up to
500,000 training examples, 500,000 cached responses and 900,000 queued
observations. Actual disk usage is governed independently by occupied SQLite
pages and estimated queue bytes. Safety/regex activation thresholds and CPU
concurrency do not grow with the budget.

See [ML capacity and scaling](docs/ML_SCALING.md) for size profiles, migration
from the older ML variables and what is excluded from the budget.

**Safe observation mode:** set `MATCHLAB_ML_LIVE_REGEX_ENABLED: "false"` to keep
collecting/training locally while preventing learned regex rules from changing
the live parser. This is an emergency opt-out for native regex publication,
not a switch to disable the canonical parser. Re-enable only after examining
`GET /api/ml/status` and the local regression report.

Only `MATCHLAB_ML_SIZE_GB` is needed for local ML. The training and queue
limits are estimated numbers of retained observations, not RAM reservations;
training batches are chosen automatically. The separate bounded SQLite ML job
queue stores observations after matching. Cache hits use a small in-memory
tier: HTTP parsing/matching never opens the cache SQLite file. Cache writes run
in their own background worker, separately from training. After parser, rule
or metadata changes, old cached answers are ignored and requests are parsed/
matched again. Learning evidence is never treated as a completed match.

Check `GET /api/ml/status` to see local model and rule statistics. No separate AI server, GPU or online AI service is required.

### ML novelty gate (parser improvement, not regex duplication)

The model still trains from every parser field, including successful canonical parses, but a technical marker is only **activated as a new regex** after at least **four independent missing-field observations covering two title identities**, alongside the existing minimum of eight examples and four distinct title identities. A valid native parser handler must exist for the exact field/transformation; otherwise the candidate remains training or review evidence. This prevents standard markers like `x264`, `1080p`, `mkv` and `subbed` from inflating the active-rule count merely because they are common.

TV families require three distinct missing-coordinate observations, two distinct affected titles and two correct original-parser confirmations before activation. Conflicting coordinates never count as missing. Learned regexes still use MatchLab's original field transforms, never directly produce field values, and are shared only after local validation. Historical rules remain archived; an updated build may initially show fewer **active** rules without deleting its learned evidence.

`GET /api/ml/status` now reports `evidence_only_candidates` and an `activation_policy`. This intentionally prioritizes useful changes over the raw number of generated patterns.

### Share improvements with the community (optional)

GitHub sharing is **off by default**, even when local ML is on. MatchLab only posts proposed regex changes and selected diagnostics as comments on Issues in [Dizzybog/MatchLab](https://github.com/Dizzybog/MatchLab). It does **not** create Issues or merge PRs.

The trusted **Community Learning GitHub Action** now independently tests proposed
source-handler regex changes against the original Rust parser, known-good
torrent titles and maintainer-verified expected fields. It flags redundant and
regressive rules; only independently proven extensions can stage an exact
source-regex union and new Rust regression tests in a **human-review PR**. 
Neither an ML score nor an Issue comment can directly edit the parser or
automatically merge a PR. See [the automated regex impact reviewer](docs/COMMUNITY_LEARNING.md#automatic-native-regex-impact-review).

#### 1. Create the right GitHub token

**If you own the upstream MatchLab repository** (or it is selectable in your GitHub token settings), use a **fine-grained personal access token**:

1. Log into GitHub and open **Settings → Developer settings → Personal access tokens → Fine-grained tokens → Generate new token**. [Open GitHub's token settings](https://github.com/settings/personal-access-tokens/new).
2. Name it `MatchLab community sharing` and set a limited expiration (for example 90 days).
3. Set **Resource owner** to the account that owns the upstream repository, **Dizzybog**. Under **Repository access**, choose **Only select repositories** and select **MatchLab**.
4. Under **Repository permissions**, set **Issues → Read and write**. **Metadata → Read-only** is automatic. No **Contents**, **Actions**, **Administration** or **Pull requests** write access is needed.
5. Select **Generate token**, then copy the secret immediately. GitHub does not show its full value again.

**If you are an external contributor and cannot select `Dizzybog/MatchLab`**, GitHub currently cannot use a fine-grained PAT to contribute to public repositories outside your account/organization. Use **Settings → Developer settings → Personal access tokens → Tokens (classic) → Generate new token (classic)** instead ([open classic token settings](https://github.com/settings/tokens/new)). Select **`public_repo` only**, set a short expiration and generate the token. **Security tradeoff:** `public_repo` is broader than single-repository access and can authorize actions across public repositories, so treat this token as sensitive and revoke it when you stop sharing. **Do not enable `repo`** unless you separately need private-repository access.

References: [GitHub's PAT guide](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens) and [Issue-comment API permissions](https://docs.github.com/en/rest/issues/comments#create-an-issue-comment).

#### 2. Keep the token out of Compose and Git

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

#### 3. Restart and verify token authentication

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

See [Machine Learning and Community Learning](docs/COMMUNITY_LEARNING.md) for supported patterns, limitations, privacy considerations, validation rules and contributor setup.

## Configuration and security

| Setting | Default | Description |
| --- | --- | --- |
| `MATCHLAB_DB` | `/data/metadata.sqlite3` | Local metadata database |
| `MATCHLAB_HOST` | `0.0.0.0:8765` in Docker | API listener |
| `MATCHLAB_METADATA_REFRESH_HOURS` | `168` | Metadata refresh interval |
| `MATCHLAB_MAX_CONCURRENT_MATCHES` | `4` | Maximum concurrent matches (example uses 2) |
| `MATCHLAB_ML_SIZE_GB` | `0` | Single local ML control: 0=off, 1–128 GiB total budget; automatically scales examples, cache and queue |
| `MATCHLAB_ML_LIVE_REGEX_ENABLED` | `true` | Optional emergency opt-out: train without activating new regex rules |
| `MATCHLAB_ML_CACHE_ENABLED` | `true` when ML is on | Optional opt-out for cached results |
| `MATCHLAB_ML_CACHE_STATE_FILE` | `/data/ml-result-cache.sqlite3` | Advanced cache-path override |
| `MATCHLAB_COMMUNITY_LEARNING_ENABLED` | `false` | Enable optional GitHub reporting |
| `MATCHLAB_COMMUNITY_GITHUB_TOKEN` | Unset | Fine-grained PAT with Issues Read/write on upstream (owner), or classic `public_repo` PAT for external contributors; validated on startup |

More settings are documented in [Metadata lifecycle](docs/LOCAL_METADATA.md), [API reference](docs/API.md) and [Community Learning](docs/COMMUNITY_LEARNING.md).

**The API and dashboard have no built-in login.** The example only binds the port to localhost. Use an authenticated HTTPS reverse proxy before exposing it publicly. Learning and diagnostic history can contain private release names; protect the Docker volume and never commit the GitHub token.

## Build and test

```bash
git clone https://github.com/Dizzybog/MatchLab.git
cd MatchLab
RUSTFLAGS="-D warnings" cargo test --workspace --locked
docker build -t matchlab:local .
```

For parser details and compatibility tests, see [Release parser audit](docs/RELEASE_PARSER_AUDIT.md). For anime-specific cases, see [Anime parser research](docs/ANIME_PARSER_RESEARCH.md).

## Credits and license

MatchLab is an independent Rust project informed by:

- [StremThru](https://github.com/MunifTanjim/stremthru) — metadata and matching ideas.
- [Comet](https://github.com/g0ldyy/comet) — release parsing and airdate concepts.
- [AIOStreams release parser](https://github.com/AIOStreams/release-parser) (MIT) — parsing patterns and release-name validation.
- [TheBeastLT / parse-torrent-title](https://github.com/TheBeastLT/parse-torrent-title) (MIT) — Torrentio-related parsing cases.
- [Rakun](https://github.com/lowlighter/rakun) (MIT) — anime naming conventions.
- [FTRL-Proximal research](https://research.google/pubs/ad-click-prediction-a-view-from-the-trenches/) — approach used by the local Rust learner.

MatchLab is not affiliated with these projects. Their authors retain their respective credits and licenses. MatchLab is licensed under **GPL-3.0-or-later**; metadata sources have their own terms.
