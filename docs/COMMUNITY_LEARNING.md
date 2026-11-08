# MatchLab Machine Learning and Community Learning

**Help MatchLab learn how torrents are named, without surrendering the
speed, privacy or deterministic checks of the Rust matcher.** This design
has two separately controlled capabilities: offline local learning and
optional GitHub community contributions. Both default to **off**.

No remote AI service, external training API, model download, account or
GitHub connection is needed for the local learning engine. The official
AMD64 and ARM64 Docker image contains the Rust application, a tiny ONNX
bootstrap model and a pinned CPU-only ONNX Runtime library. The image
also supports normal parsing/matching with no ML worker initialized.

## Goals, scope and what the ML model really does

MatchLab compares release filename parsing with IMDb/AniDB/TVDB matches.
The local worker observes field-level disagreements and groups recurring
patterns by release markers, media fields and matching outcomes. Examples:

| Release | Parser/matcher difference | Learning action |
| --- | --- | --- |
| `Bleach_300` | An absolute episode is recognized, but its IMDb/AniDB coordinates are missing | Look up exact mapping; do not invent IDs |
| `Bleach S16` | Season parsing succeeds, but AniDB range remains incomplete | Investigate crosswalk, not parser regex |
| `Series.EPIS.300` | Existing regex misses or misreads an alternative episode marker | Learn a bounded `EPIS` regex if many independent crosswalks confirm it |
| `Film.2024` | Torrent year differs from IMDb original release year | Record a possible mismatch; NOT proof of incorrect matching |
| Other codecs, languages, resolution, quality | A likely field marker is missing or conflicts | Record a field-level anomaly for future verified rules |

The first **live learning rule family** is tightly scoped to alternative
absolute-anime-episode markers. It is implemented in Rust with
`regex` (linear-time matching), and may propose an exception to a
wrong built-in parser field only after the current metadata crosswalk
uniquely confirms the correct identity and episode. Regexes are
constructed from an allowlisted template; arbitrary Issue text cannot
supply PCRE2 code. Other parser fields are observed and scored, but
do **not** yet auto-generate arbitrary regex replacements.

The lightweight ONNX ranker currently uses *fixed bootstrap weights*.
This is **not a continuously retrained neural network**, and scores are
not calibrated probabilities. We do not describe a score as 99% certain
or treat it as ground truth. Rule activation and GitHub proposals use
independent evidence counts, diversity, current metadata, negative
tests, and regression checks. The ONNX score only helps prioritize
anomaly patterns. Future supervised training can update the model
after independently verified positive and negative labels exist.

## Opt-in modes

| Mode | Environment | Data sharing | ONNX and local worker |
| --- | --- | --- | --- |
| Standard (default) | No ML variables | None | Dormant |
| Local learning | `MATCHLAB_ML_ENABLED=true` | None; works offline | One CPU worker, local cache and live regex overlays |
| Community Learning | Local learning + `MATCHLAB_COMMUNITY_LEARNING_ENABLED=true` + private GitHub token | One bounded batch per 24h to GitHub | Same independent local worker |

For local-only learning:

~~~yaml
services:
  matchlab:
    image: ghcr.io/dizzybog/matchlab:latest
    environment:
      MATCHLAB_ML_ENABLED: "true"
      MATCHLAB_ML_MAX_STORAGE_GB: "1"
      MATCHLAB_ML_CACHE_ENABLED: "true"
    volumes:
      - metadata:/data
~~~

Only `MATCHLAB_ML_ENABLED` is required. An ML-disabled container
never creates the learning worker, initializes the ONNX CPU runtime, or
opens the community uploader. A GitHub token by itself is insufficient
to activate anything. The Rust matcher still works without a token.

### Technical resources

- The official Docker image bundles the CPU ONNX Runtime binary for each
  platform and an ONNX graph generated entirely in Rust at build time.
  Both assets are dormant until opt-in. CI performs real inference on
  **linux/amd64** and **linux/arm64**.
- Local observations are collected in bounded background batches; ONNX
  inference uses **one CPU thread**, not GPUs. No Python backend or Ollama.
- The local SQLite file is `/data/local-learning.sqlite3` by default,
  and the logical storage target `MATCHLAB_ML_MAX_STORAGE_GB`
  defaults to **1 GiB**, supporting 1–128 GiB. The budget is a
  best-effort on-disk quota, not an exact hard filesystem partition.
- Persistent caching can be disabled independently with
  `MATCHLAB_ML_CACHE_ENABLED=false`; rule learning can remain active.
- Ordinary parser/matcher hot paths do not execute ONNX. Compiled
  lightweight community-approved regex patterns can run even when
  local ML is off, but still require current metadata validation
  before changing final episode coordinates.

## Local evidence and rules

The Rust worker scans retained diagnostic history in bounded chunks.
A candidate is relevant only if its release marker maps to an
independently verified IMDb and AniDB/TVDB episode crosswalk, its title
matches the selected canonical IMDb title exactly after normalization,
and the current base parser is demonstrably missing or misreading that
absolute episode. The evidence includes a private release title, exact
coordinate tuple and a stable dedup fingerprint. No guess is treated as
a training label merely because it recurs.

For a **local live regex**, the minimum is **8** checked observations,
at least **2** IMDb identities and **3** distinct episode numbers.
The rule must satisfy a safe marker grammar, pass negative probes,
be within the maximum rule count, and remain reproducible against
current metadata and parser code. Live activation is a hot-reload
of a small set of compiled rules; a proposed episode override
does not become a final SID/ASID until the matcher proves the
crosswalk for that particular request.

A rule with insufficient or stale evidence is *quarantined* or
marked *obsolete*, not returned as an active rule. Old evidence is
retained for later revalidation as a cache rather than deleted
immediately. Historical cache records are never final answers.

### Docker/image and metadata upgrades

Each completed parse/match cache entry is bound to an exact
source/build, active regex revision, metadata revision and expiration.
Changed or obsolete cache entries **never serve requests**, even
when still stored. They are reparsed and rematched on demand using
current Rust code and metadata, then cached in the new namespace.
There is no CPU-heavy full-cache replay on startup.

On an image or metadata update, the local worker rechecks learned
regex evidence against current code and crosswalks before activating
it. Obsolete candidates and their observations remain available for
future reconsideration; if the global learning database exceeds its
quota, inactive obsolete records are evicted first, including their
supporting examples, with SQLite foreign-key cascades. Current
verified active rules are protected. Incompatible learning epochs
invalidate old labels without automatically erasing their history.

## Contribute to the GitHub community (optional)

**Nothing leaves a local-only installation.** To contribute:

1. Use a current official MatchLab image. Historical releases built
   from upstream `main` can still report compatible, unresolved errors;
   the main-branch reviewer discards problems already fixed.
2. Create a personal GitHub token:
   - If you are a repository member, use a [fine-grained PAT](https://github.com/settings/personal-access-tokens/new)
     with **Dizzybog/MatchLab → Issues: read/write** only.
   - If you are an external public contributor, a
     [classic PAT](https://github.com/settings/tokens) with
     `public_repo` may be necessary; that scope is broader.
     Use a dedicated, restricted account if appropriate.
   Do not post your token in an Issue or chat.
3. Store the actual token in an **uncommitted local `.env`** or a
   secret injector and enable both flags:

   ~~~yaml
   environment:
     MATCHLAB_ML_ENABLED: "true"
     MATCHLAB_COMMUNITY_LEARNING_ENABLED: "true"
     MATCHLAB_COMMUNITY_GITHUB_TOKEN: ${MATCHLAB_COMMUNITY_GITHUB_TOKEN}
   ~~~

4. Redeploy and check `GET /api/community/status`: the response
   should show `local_learning_enabled: true`, `enabled: true`,
   and `ready: true`. Disable the sharing flag and redeploy to
   opt out; revoke the token if no longer needed.

**Privacy warning:** Local evidence and complete cache results can
contain private release names; keep `/data` private. Public
GitHub Issues and PRs may still reveal release *basenames*, even
after rejecting paths, URL strings and obvious secrets. Contribute
only filenames you are willing to make public. The client never
uploads complete metadata SQLite files, torrent contents, tracker
credentials, or the GitHub token.

### The 24-hour contribution protocol

- GitHub Actions creates/reuses **one shared Issue per UTC day**.
  Clients only submit comments to the dated Issue created by the
  `github-actions[bot]`; they never create their own Issues.
- Each explicitly opted-in client collects up to **16** unique items
  over a **24-hour window**, then attempts at most one batch comment.
  A batch may contain strict anomaly observations and rule proposals.
  Clients mark attempted batches and fingerprints before submitting
  to avoid reposting after an ambiguous network timeout. This means
  an individual batch may be lost rather than duplicated.
- The local ledger survives upgrades and prevents resending the same
  fingerprint. A GitHub Release asset maintains global cross-install
  deduplication. GitHub may still impose API/abuse limits.
- Compatible **v2 anomaly** reports continue to be reviewed. **v3
  batches** can also carry verified local regex proposals. GitHub
  validates both independently, never trusting a claimed SHA as
  cryptographic proof of a running binary.
- The source commit must be an ancestor of current upstream main.
  Reported parser anomalies are replayed by the **latest Rust parser**;
  identity/crosswalk claims need independent verification, because
  no client's reported ID is a trustworthy label by itself.

### How a high-evidence regex becomes a PR

A local regex must be **active and independently revalidated** on the
current source and metadata before it can be exported. Upstream PR
screening has additional requirements: at least **12** independently
verified observations, **4** distinct IMDb identities, **5** episode
numbers, and multiple non-private example filenames. The uploader
sends a generated, restricted regex plus bounded evidence — not raw
user-authored PCRE2 expressions.

The daily trusted GitHub Actions workflow:

1. Loads the previous day's Issue comments as **untrusted JSON data**;
   rejects malformed schemas, incompatible epochs, non-mainline SHAs
   and already seen fingerprints.
2. Regenerates the canonical regex from its allowlisted release
   marker. Checks sufficient evidence diversity and sanitized names.
3. Uses a Rust **current-parser** screening program to reject
   redundant or unsafe rules, and negative probes to catch obvious
   collisions. It adds at most **one** rule to a proposed versioned
   `learning/community_rules.json` per daily run.
4. Runs the Rust test and all-target check on the candidate before
   opening a GitHub pull request. The bot account must have
   permission to create PRs. **All PRs require human review** of the
   claimed IMDb/AniDB evidence and are **never auto-merged**.
5. Once the maintainer merges a verified rule, it is compiled into
   the next official image. These community-curated patterns work
   even without local ML; final IMDb/AniDB episode values still pass
   the current crosswalk and normal production gates.

**Important limitation:** GitHub currently replays parser output
against the current code but does not independently download a
full IMDb/AniDB metadata database in the bot review job. Thus a
valid-looking mapping supplied by a client is still a **claim**,
not authoritative proof. The bot may open a review PR but cannot
approve or merge a proposed rule autonomously. Also, the current
local ONNX model is a deterministic bootstrap ranker; claims of
calibrated 99% certainty would be misleading.

## Maintainer setup

After merging the implementation PR and verifying the Docker image:

1. Enable **Issues**, **GitHub Actions** and **GitHub Releases** for
   the repository.
2. In **Settings → Actions → General**, allow GitHub Actions to
   **create pull requests**. The daily workflow requests
   `contents: write`, `issues: write` and `pull-requests: write`;
   no permission to merge or approve is used. Branch protection
   must still require review and CI as desired.
3. Verify the Docker publishing workflow supplies
   `MATCHLAB_BUILD_SHA` at compile time for the official image.
   Compatible older builds can submit hypotheses but are rechecked
   against current main.
4. From **Actions → Community learning review**, choose **Run
   workflow** to seed today's shared Issue, or leave the UTC 00:23
   schedule enabled. Scheduled jobs can be delayed or disabled on
   inactive repositories; monitor failed runs.
5. Check the daily review artifact (7-day retention), rule PRs and
   `community-learning-ledger` GitHub Release asset. If GitHub
   rejects PR creation due to repository settings, fix the setting
   and re-run the workflow; never embed a maintainer PAT in Docker.
6. Independently verify proposed rule examples, negative tests,
   and actual IMDb/AniDB crosswalks before approving and merging
   any bot PR. Review the scope of every new regex.

**No external Learning Hub server is required.** Python in the hosted
GitHub Actions review job is not part of the MatchLab image or its
weak-host runtime; core parser/matcher, local learning, ONNX
inference and local regex application all run in Rust.

## Troubleshooting and security boundaries

- `MATCHLAB_ML_ENABLED=false`: no local worker, ONNX session or
  learning-specific GitHub traffic. The usual matching API works.
- Local ML on, sharing off: no GitHub traffic related to learning.
- Sharing on without local ML/token/official SHA: uploader refuses
  to start. See `/api/community/status` and container logs.
- ONNX asset missing or incompatible: learning observation counts
  continue without ONNX scoring; the core matcher is unaffected.
- An ambiguous episode, an unverifiable IMDb identity, stale
  metadata, or insufficient independent evidence cannot
  activate a local override or authorize a GitHub PR.
- An unsupported parser/label epoch quarantines old learned
  evidence; it is not silently promoted to new training labels.
- A GitHub token is not remote attestation. Contributors can forge
  reports; all inputs remain untrusted until independent review.
- The runtime regex language is restricted to safe, bounded,
  linear-time templates. No remote Issue can execute shell,
  SQL, Rust or arbitrary regex bytecode.
- The local learning quota uses SQLite page measurements and
  background maintenance; it is a logical best-effort storage
  budget, not a hard OS disk quota.

For the matching API, metadata behavior and debugging see
[API documentation](API.md), [metadata lifecycle](LOCAL_METADATA.md)
and [release parser audit](RELEASE_PARSER_AUDIT.md).
