# MatchLab

**MatchLab** is a self-hosted Rust service that recognizes movies, TV series, and anime from torrent filenames. Send a release name to its HTTP API to get an **IMDb ID**, verified episode coordinates, and—when available—an **AniDB ID**.

It can parse names such as `Breaking.Bad.S02E03.1080p.WEB-DL`, recognize season packs, and handle anime episode mapping. It **does not** search for or download torrents.

## Features

- **Fast title matching:** Tantivy indexes IMDb titles, alternate titles, and AniDB aliases. MatchLab ranks exact titles, country aliases, year, and media type using deterministic rules.
- **Anime support:** AniDB, animeApi, and Anime-Lists provide verified identities and episode conversions, including split seasons and absolute episode numbering.
- **Full release parsing:** Extracts title, year, season, episodes, resolution, languages, release tags, and more. Supports filenames inside folder paths.
- **Easy deployment:** Native Rust server with Docker, an HTTP API, a dashboard, and automatic metadata refresh.
- **Safe updates:** SQLite stores metadata relationships; Tantivy generations are built and validated before metadata is switched.

## Machine Learning — learn new rules and improve existing regex

MatchLab's optional **Machine Learning (ML)** subsystem learns from real
torrent-release filenames while the normal parser and IMDb/AniDB matcher keep
working in Rust. All model training and regex validation happen in a
background worker. **The model is not invoked for each HTTP request.**
No GPU, Python service, ONNX Runtime, Ollama or external ML provider is needed.

### What can it learn?

The learner audits **every parsed field**: title, year, seasons, episodes,
resolution, codec, quality, audio, languages, HDR, subtitle/dub flags, file
extension and additional release metadata. It also examines selected IMDb
and AniDB matches for discrepancies. An observed disagreement is **not**
automatically a correct label: torrent filenames, parser output and IMDb
original-release years can each be misleading.

| Learning area | What is enabled today |
| --- | --- |
| **All parser fields** | Detect omitted, unusual or conflicting values; record bounded, hashed observations for review |
| **Technical release fields** | Learn literal-marker regexes that fill missing fields or, with stronger evidence, correct existing values |
| **Ordinary TV seasons and episodes** | Learn explicit `S02E07`, `2x07` and `Season 2 Episode 7` conventions; fill missing values and conditionally correct a conflicting single coordinate |
| **Absolute anime episodes** | Learn new marker conventions, subject to independent AniDB/TVDB verification before they affect a selected match |
| **Existing parser regex** | Inspect the full generated handler catalogue and safely compile selected, evidence-backed broader replacement patterns in the background |
| **Title, year, unmarked episode, IMDb/AniDB identity** | Detect and rank inconsistencies; do **not** invent an identity or automatically rewrite an ambiguous result without independent verification |

For technical fields, at least **8 concordant examples from 4 different
titles** can activate a missing-field rule; replacing a value requires
**16 examples, 6 titles, and 2 original-parser confirmations**. The
ordinary-TV learner additionally requires at least two confirmations
from the original parser, with stronger evidence to correct an existing
single season or episode. It never overwrites episode packs or ambiguous
multi-marker releases.

MatchLab can use learned rules in **two ways**. An *additional rule*
recognizes a pattern the original parser missed. A *source-handler
improvement* broadens a verified existing regex while preserving that
handler's field, transformation, ordering and original matching branch.
For simple technical patterns, the worker can compile and atomically
hot-swap the improved handler; the old compiled parser remains available
as canonical training truth. More complex or ambiguous regex changes
remain review-only. A trained model score alone never authorizes a change.

### How it works

1. **Observe** the original filename and original parser output, not a
   cached or ML-modified answer.
2. **Learn** with a small online FTRL-Proximal model implemented in Rust,
   supplemented by explicit marker and metadata validators.
3. **Compare** any candidate against existing source regex, independent
   title identities, conflicting markers and negative examples.
4. **Compile once** in the background and hot-load only acceptable rules.
   Ordinary requests use precompiled regex, with no ML inference.
5. **Version the cache.** A parser, learned-rule or metadata change
   invalidates completed answers. The next query reparses and rematches;
   training evidence never becomes a cached ground-truth answer.
6. **Optionally contribute** verified proposals through GitHub. Community
   sharing is separate from local ML and never auto-merges a PR.

### Simple Docker Compose settings

Set `MATCHLAB_ML_ENABLED=true` in your `.env` to turn learning on:

```dotenv
MATCHLAB_ML_ENABLED=true
MATCHLAB_ML_TRAINING_MAX_EXAMPLES=500000
MATCHLAB_ML_MAX_STORAGE_GB=10
MATCHLAB_ML_CACHE_MAX_ENTRIES=200000
```

The supplied [compose.yaml](compose.yaml) reads these four settings
automatically. **Only the enabled switch is required**; the other
settings have defaults. `MATCHLAB_ML_TRAINING_MAX_EXAMPLES` controls
retained validated *field observations*, not a fixed RAM allocation or
the number of distinct torrent titles. The cache uses exact keyed SQLite
lookups, while normal IMDb search remains in Tantivy. Successes and
no-matches have separate TTLs; stale results are recomputed.

For the learning status, inspect `GET /api/ml/status`. Local learning
never posts to GitHub. If you also want to contribute publicly, enable
`MATCHLAB_COMMUNITY_LEARNING_ENABLED=true` and provide your own private
`MATCHLAB_COMMUNITY_GITHUB_TOKEN`. The upstream GitHub intake now accepts **all three other
regex proposal types**: new technical-field rules, ordinary-TV season/episode
rules and modifications to existing generated handler regex, in addition to
verified anime markers. Each submitted proposal contains the actual regex;
source modifications include **both original and proposed patterns** plus
their original handler index, field, transform and evidence counters.
The daily GitHub Action opens at most one human-review PR. Technical/TV/source
proposals enter `learning/community_regex_proposals.json` as **review-only
suggestions**; they are not silently installed on other users' systems by
merging an unverified report. Independently tested and approved changes must
subsequently be integrated into the parser. Only approved anime marker rules
are packaged automatically via `learning/community_rules.json`.

**[Full learning architecture, limitations and contribution guide](docs/COMMUNITY_LEARNING.md)**

## Dashboard

<img width="1434" height="4096" alt="1000502509" src="https://github.com/user-attachments/assets/e46326cc-3ce7-40ac-be87-4d14bf3ad952" />


## Quick start

Requires Docker and Docker Compose. Save the following as `compose.yaml` (or use the [compose.yaml](compose.yaml) included in this repository):

```yaml
services:
  matchlab:
    image: ghcr.io/dizzybog/matchlab:latest
    container_name: matchlab
    restart: unless-stopped
    cpus: 1.0
    mem_limit: 6g
    pids_limit: 256
    environment:
      RUST_LOG: matchlab_server=info
      MATCHLAB_MAX_CONCURRENT_MATCHES: "40"
    ports:
      - "127.0.0.1:8765:8765"
    volumes:
      - metadata:/data
    logging:
      driver: json-file
      options:
        max-size: "10m"
        max-file: "3"

volumes:
  metadata:
```

Start and check the service:

```bash
docker compose up -d
docker compose logs -f matchlab
curl http://127.0.0.1:8765/health
```

On first startup, MatchLab downloads **seven public metadata feeds** and builds its local SQLite database and Tantivy index. This requires outbound HTTPS access and sufficient free disk space. The HTTP listener and dashboard are available while Tantivy is indexing, so `/health/live` can return **200** before matching is ready. `/health` and `/health/ready` return JSON with **503** and status `initializing`, `indexing` or `error` until the index is ready, then **200** and `ready`. The dashboard displays the phase. Matching and alias lookups are gated until ready; the client does not mistake proxy HTML for JSON. Later metadata refreshes preserve the last published data if an import fails.

For updates, run `docker compose pull && docker compose up -d`.

> **Resource limits:** The example restricts MatchLab to **1 CPU, 6 GB RAM, and 256 processes/threads**, with up to **40 simultaneous matching operations**. These are starting values, not verified minimum requirements. Increase the RAM/CPU limits if full metadata imports exceed them. Disk space must cover both the active and incoming metadata/index generations.

## HTTP API

MatchLab listens on port **8765**. Both `GET` and `POST` accept `title` (or `t_title`), and matching works without calling the parser first.

| Endpoint | Description |
| --- | --- |
| `GET /match`, `POST /match` | Match a release to IMDb and optional AniDB coordinates |
| `GET /alias`, `POST /alias` | Exact title/ID lookup across IMDb, AniDB, TVDB, MAL, AniList and Kitsu |
| `GET /episode/resolve`, `POST /episode/resolve` | Verified IMDb season/episode ↔ AniDB episode conversion |
| `GET /parse`, `POST /parse` | Parse the full release name without metadata lookups |
| `POST /debug/match` | Show candidate ranking and episode-mapping details |
| `GET /api/debug/parsing/status` | Authenticated read-only history cursor and retention information |
| `GET /api/debug/parsing` | Authenticated incremental parser diagnostics and anomaly hints |
| `GET /api/debug/parsing/{id}` | Authenticated full diagnostics for one retained request |
| `GET /health` | Check whether indexed metadata is ready |
| `GET /health/live` | Lightweight liveness check |
| `GET /` | Open the dashboard and search history |

### Match a release

```bash
curl -X POST http://127.0.0.1:8765/match \
  -H "Content-Type: application/json" \
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

**Strict production year on `/match` only:** Parsed torrent years must match the IMDb
production year **exactly**, not within ±1. When the year is missing or
different, a candidate requires a parsed series season, linked anime
identity, or the parser's explicit **quality** field (for example WEB-DL,
WEBRip or BluRay). **Resolution alone is insufficient**: 1080p/2160p,
codecs and file extensions do not qualify. Anime filenames with only a local
episode number can default to a tentative first-season SID when the
crosswalk cannot resolve it, but never claim an unverified ASID.
Verified absolute numbering and named anime parts take priority.

**Dashboard behavior:** The dashboard tester (`POST /debug/match`) intentionally
does not apply the public API's production-year/quality eligibility gate.
Entering `Fight Club` alone will still return ranked title matches without
requiring a year, season, anime identity or release-quality tag. The public
`GET/POST /match` endpoints retain all the strict checks.

`/match` **always returns exactly these four fields**. Movies use the IMDb title ID in both SID fields; TV episodes add season/episode coordinates. Verified anime episodes may also include AniDB IDs. If nothing can be matched reliably, the corresponding values are `null`.

### Find all anime names and third-party IDs

```bash
curl -G --data-urlencode 'imdb_id=tt0388629' http://127.0.0.1:8765/alias
curl -G --data-urlencode 'anidb_id=69' http://127.0.0.1:8765/alias
curl -G --data-urlencode 'kitsu_id=987' http://127.0.0.1:8765/alias
curl -G --data-urlencode 'mal_id=21' http://127.0.0.1:8765/alias
curl -G --data-urlencode 'query=One Piece' http://127.0.0.1:8765/alias
```

`/alias` returns all matching `imdb_ids` and `anidb_ids`, plus
original aliases with language, source and optional year. Search is
**exact after MatchLab normalization** (not fuzzy or prefix search).
`GET` and `POST` accept the same fields; use `q` for automatic ID
or text detection. Alias objects are paginated; use `limit`/`offset`
with `total`/`has_more` for large result sets. See
[API documentation](docs/API.md#alias-lookup-get-alias-post-alias).

### Automatic SID + ASID in a normal match

```bash
curl -G --data-urlencode 'title=One Piece S21E03' \
  http://127.0.0.1:8765/match
```

With a verified One Piece crosswalk this returns
`sid_start=sid_end=tt0388629:21:3` and `asid_start=asid_end=69:894`.
The same crosswalk is applied automatically after late episode
identification (such as date matches); no second lookup is required.

### Convert IMDb ↔ AniDB episode coordinates

```bash
curl -G -d 'imdb_id=tt0388629' -d 'season=21' -d 'episode=1' \
  http://127.0.0.1:8765/episode/resolve
curl -G -d 'anidb_id=69' -d 'episode=892' \
  http://127.0.0.1:8765/episode/resolve
```

Only verified crosswalk data produces a result. Ambiguous reverse matches
return all candidates, with `selected: null`, rather than guessing.

### Parse or debug

```bash
curl 'http://127.0.0.1:8765/parse?title=Breaking.Bad.S02E03.1080p'
curl -X POST http://127.0.0.1:8765/debug/match \
  -H "Content-Type: application/json" \
  -d '{"title":"One.Piece.E768-E1064"}'
```

The full parser exposes all recognized release details. Debug output explains the selected title, alternatives, and episode conversions. MatchLab also supports title inheritance from folder paths and airdate-based TV matching via Cinemeta when verified episode information is available.

Unmarked numeric anime releases, such as `Jojo's bizarre adventure stone ocean 27`, now expose an **unverified** candidate in `/parse`. `/match` promotes it only after an exact, unambiguous anime alias is found in the Tantivy index and reuses the existing canonical IMDb/AniDB episode checks. Movie titles with trailing numbers are not treated as verified episodes by the parser alone.

The optional Torrentio-inspired parsing compatibility layer recognizes selective Unicode release-marker variants (`S01Е02`, `720р`), written Chinese season ordinals and conservatively detected fractional anime episodes (`12.5`), without changing the native Rust engine or adding runtime dependencies. A `half_episode` flag is descriptive and does not itself prove a canonical IMDb/AniDB episode.

The native Rust release parser now includes AIOStreams-inspired TV/anime episode-title extraction, Rakun fansub release conventions, extra network/provider tags, accessibility/description metadata and extended language guards. It keeps the existing parser/matcher split, without a JavaScript runtime. See the [full parser audit](docs/RELEASE_PARSER_AUDIT.md) and [anime research](docs/ANIME_PARSER_RESEARCH.md) for exact coverage and documented exceptions.

**[Detailed API documentation](docs/API.md)** covers response fields, season packs, episode ranges, anime, airdate matching, and missing/ambiguous metadata.

## Metadata and settings

IMDb basics, alternative titles, episodes, ratings, animeApi, Anime-Lists, and AniDB titles are refreshed by default **every seven days**. Metadata is persisted in `/data`; the active Tantivy index survives container restarts.

| Variable | Default | Purpose |
| --- | --- | --- |
| `MATCHLAB_HISTORY_DB` | `/data/history.sqlite3` | Separate history database; latest 1,000 request details and independent 14-day statistics |
| `MATCHLAB_DB` | `/data/metadata.sqlite3` | Metadata SQLite file |
| `MATCHLAB_HOST` | `0.0.0.0:8765` in Docker | HTTP listener |
| `MATCHLAB_METADATA_REFRESH_HOURS` | `168` | Refresh every 1–720 hours |
| `MATCHLAB_MAX_CONCURRENT_MATCHES` | `4` | Limit simultaneous match operations (1–64) |
| `MATCHLAB_DEBUG_API_TOKEN` | Unset (disabled) | Enable authenticated, read-only parsing diagnostics with a random token of at least 32 bytes |
| `MATCHLAB_COMMUNITY_LEARNING_ENABLED` | `false` | Separate opt-in for deduplicated GitHub anomaly and high-evidence regex proposals |
| `MATCHLAB_COMMUNITY_GITHUB_TOKEN` | Unset | Contributor-owned GitHub token with permission to comment on public upstream Issues; members may use fine-grained PAT, outsiders need classic `public_repo` |
| `MATCHLAB_ML_ENABLED` | `false` | Opt in to the Rust-only local learning worker (never enables sharing by itself) |
| `MATCHLAB_ML_MAX_STORAGE_GB` | `1` | Shared best-effort learning DB disk budget in GiB (1–128); separate from training example count | 
| `MATCHLAB_ML_TRAINING_MAX_EXAMPLES` | `12000` | Retain 100–2,000,000 current-build validated field observations; unpromoted old evidence rotates when full |
| `MATCHLAB_ML_CACHE_MAX_ENTRIES` | `100000` | Maximum persistent parse/match cache rows (100–2,000,000), enforced by background cleanup |
| `MATCHLAB_ML_CACHE_TTL_HOURS` | `720` | Repeated successful parse/match response lifetime (1–720 h); build/rule/metadata changes invalidate earlier |
| `MATCHLAB_ML_UNMATCHED_CACHE_TTL_MINUTES` | `30` | Short no-match result lifetime (1–1440 min); changes invalidate earlier |
| `MATCHLAB_ML_CACHE_ENABLED` | `true` when local learning is on | Reuse versioned parser and matcher results; disabled when local learning is off |
| `MATCHLAB_ML_STATE_FILE` | `/data/local-learning.sqlite3` | Shared local SQLite file for learned rules, evidence, pattern statistics and result cache |
| `MATCHLAB_COMMUNITY_STATE_FILE` | `/data/community-learning.sqlite3` | Bounded pending batch queue, persistent fingerprints and 24h submission state |
| `MATCHLAB_METADATA_URLS` | Built-in HTTPS feeds | Optional seven-source override; legacy six-source lists are supported |
| `RUST_LOG` | `matchlab_server=info` | Logging verbosity |

**History and statistics retention:** The dashboard keeps at most **1,000 latest individual requests** (including full diagnostics), regardless of age. Its **Requests, Matched, Unmatched, Errors, and average latency** statistics cover all requests from the **last 14 days (UTC, minute precision)**, not just those 1,000 rows. A separate SQLite `request_minute_stats` table accumulates counters by minute; the dashboard activity chart groups them into daily totals. During the first startup after upgrading, the last 14 days are backfilled from any existing legacy request history **before** old request rows are removed. An indexed SQLite trigger evicts only the oldest request detail row on each insert once the 1,000-row cap is reached; it never alters aggregate statistics. A background maintenance task additionally runs at startup and every five minutes to remove bulk legacy history and expire aggregate buckets older than 14 days, off the asynchronous HTTP executor. This does not modify IMDb metadata. SQLite generally reuses freed pages instead of immediately shrinking the existing file; run `VACUUM` during a maintenance window once after upgrading if reclaiming disk space is important. Back up a live SQLite database using a consistent backup method, not a direct file copy.

For importer options and the exact source URLs, see [Metadata lifecycle](docs/LOCAL_METADATA.md). For index internals and crash-safety considerations, see [Tantivy architecture](docs/TANTIVY_RUNTIME.md).

**Security:** The optional parsing debug feed requires its own bearer token and is unavailable when `MATCHLAB_DEBUG_API_TOKEN` is absent or shorter than 32 bytes. The remaining API and dashboard have no built-in authentication. This example binds the port to **localhost**; put the service behind an authenticated HTTPS reverse proxy before exposing it over a network. Debug history may contain private torrent filenames. Use HTTPS, protect the other endpoints at the proxy, and never put the token in a URL. The debug feed is a polling surface, not an autonomous reviewer or a scheduled PR author. [Feed usage and cursor semantics](docs/API.md#read-only-parsing-diagnostics-feed).

### Native Rust model and local opt-in

The official image bundles all necessary Rust code for **online FTRL-Proximal
training and inference**, and no extra ML assets. The worker is dormant by
default (`MATCHLAB_ML_ENABLED=false`); enabling it uses a bounded, one-thread
background loop. Model weights and hashed training-example deduplication
persist in `/data/local-learning.sqlite3`. The model receives positive labels
only from the original parser and independently interpretable raw markers,
not its own previous predictions. Incorrect field assignments provide
counterfactual negative labels. The model is a shadow-only ranker: it never
directly replaces an IMDb/AniDB result, changes episode coordinates or
authorizes a regex. Rule promotion still requires separate verifiers.
GitHub reporting additionally requires its own explicit opt-in and token.

### Manual native ML benchmark

For an **offline synthetic CPU smoke benchmark** of the native FTRL learner,
its inference path, and the canonical parser, build the PR's source and run:

```bash
docker build -t matchlab:pr69 .
docker run --rm --entrypoint /usr/local/bin/matchlab-server matchlab:pr69 benchmark-native-ml
```

The CLI prints JSON times in milliseconds for 10,000 synthetic samples per
stage. These are **not** representative real-torrent measurements, an
end-to-end API latency test, or evidence of improved parsing precision. For
production signoff, additionally use held-out real torrent titles with field
labels, verify false-positive regressions and compare ARM64 CPU, RSS and
response latency against `main`. This opt-in benchmark is never run by the
normal API or worker.

### Online model evaluation and unknown-pattern review

The native Rust FTRL-Proximal model uses hashed character n-grams, field/marker
relationships, release context and token position. It trains only from original
Rust parser results or independently interpretable raw technical and explicit TV markers, with
counterfactual **wrong field** negatives. The model remains **shadow-only**: a model score or a repeated
observation cannot establish an IMDb ID, a season/episode coordinate, or
an unsupported technical value. Separately confirmed explicit TV marker
rules can now fill missing coordinates or correct a conflicting single value.

An approximately 20% deterministic title-group holdout evaluates whether the
model ranks each independently checked field above an intentionally wrong field.
The status API returns `holdout_pair_accuracy`, recent pairwise accuracy and
`drift_review_suggested`; **these are not calibrated probabilities or
measurements of total parsing/matching accuracy**.

A separate, bounded **review-only candidate discovery** stage identifies
unfamiliar release suffixes after known technical context. It groups candidates
across different normalized title identities and ranks them with the model.
The locally stored candidates cannot run as regex or enter GitHub proposals
without additional independent labeling and reviewer validation. The public
`GET /api/ml/status` returns aggregate candidate counts only, not raw names,
private tokens, example hashes or IMDb IDs.

Novel **independently recognizable** pixel-height markers (for example
`900p`, `1440p`, `2880p`) can instead collect safe, literal evidence
and generate field-specific local regex under the existing 8/4 fill and
16/6-plus-canonical override gates.

### Local learning storage and cache

MatchLab can keep up to **10 GiB** of local learning data and reusable
parser/matcher results, without sharing any of it with GitHub:

~~~yaml
environment:
  MATCHLAB_ML_ENABLED: "true"
  MATCHLAB_ML_MAX_STORAGE_GB: "10"
  MATCHLAB_ML_TRAINING_MAX_EXAMPLES: "500000"
  MATCHLAB_ML_CACHE_ENABLED: "true"
  MATCHLAB_ML_CACHE_MAX_ENTRIES: "200000"
~~~

The same SQLite database under `/data/local-learning.sqlite3` stores
anomaly fingerprints, independently checked regex evidence, and cached
parser/matcher JSON. The example's `500000` is a **count of retained field
observations**, not 500,000 guaranteed distinct torrent titles; releases can
supply more than one field. It does not reserve 10 GiB of RAM. Unpromoted
old observations are rotated in bounded background batches on saturation,
while evidence supporting active rules is protected. When all retained
evidence supports active rules, the worker declines new evidence instead
of silently sacrificing those rules. This is a safety limit, not a
guarantee of maximum disk-file size. Cache lookups use exact hash keys, not SQLite
full-text searches. Normal memory caching continues unchanged, even
with local learning off.

Repeated no-match requests also use the same exact-key cache but expire
after a much shorter interval (30 minutes by default). This reduces
duplicate expensive Tantivy matching while avoiding long-lived negative
results. Parse/match caches are **derived artifacts**, never training labels;
training always re-parses the raw name with the current canonical parser.

After upgrading the image, changed parser rules and metadata immediately
invalidate **completed parser and matcher results**. An obsolete cache row is
never returned, even when it is still physically stored in SQLite. On the
next request for that title, MatchLab **parses and matches again** using the
current parser, active learned rules and IMDb/AniDB metadata; only the newly
computed result enters the current cache namespace. This is lazy
recomputation, not an expensive mass reprocessing of the cache.
Historical observations, old outcomes and independently checked regex
evidence remain available for learning and audit, **not as trusted match
answers or current training labels**. Once the configured storage budget is
exceeded, maintenance first evicts obsolete learned-rule evidence,
then stale result-cache entries, then the oldest current cache entries
and least important observations. Current verified rules are protected.

**Privacy:** This persistent local cache may contain original torrent
titles and match diagnostics. Keep the Docker volume private and
avoid copying it into public diagnostics or issue reports. It never
uploads cache contents merely because local learning is enabled.

The storage cap is enforced by a background, best-effort SQLite
maintenance worker rather than expensive checks on every parser
operation. SQLite can reuse freed pages; the file's physical size
may remain high until compaction, even when logical usage is below
the configured budget.

## Contribute to Community Learning

**Help other installations learn accurate regex patterns.** Community
sharing is off by default and separate from local ML. Once enabled,
MatchLab sends at most **one batch comment per 24 hours** to one shared
GitHub Issue per UTC day; matching itself never requires GitHub.

1. Run an official MatchLab image. Compatible old mainline releases
   can contribute unresolved anomalies; GitHub rejects already-fixed bugs.
2. Create a [GitHub token](https://github.com/settings/tokens). Repository
   members may use a fine-grained token restricted to **Dizzybog/MatchLab,
   Issues: write**. For external contributors a classic token with
   `public_repo` may be needed; it has broader permissions.
3. Keep the token in a **private, uncommitted** `.env` or secret provider.
   Configure:

   ~~~yaml
   environment:
     MATCHLAB_ML_ENABLED: "true"
     MATCHLAB_COMMUNITY_LEARNING_ENABLED: "true"
     MATCHLAB_COMMUNITY_GITHUB_TOKEN: ${MATCHLAB_COMMUNITY_GITHUB_TOKEN}
   ~~~

4. Redeploy and check `GET /api/community/status`. Both
   `local_learning_enabled` and `ready` should be true. Revoke the token
   and disable sharing whenever you wish.

GitHub uses a permanent fingerprint ledger to prevent duplicate proposals
across installs and days; its trusted daily workflow validates canonical
regexes and runs current Rust tests. At most one regex PR per day is
created for **independent human review**, never merged automatically.

Submitted basenames are public and can still disclose private information,
even when paths, credentials and URLs are excluded. Local learning never
uploads anything by itself. Read the
[full contribution and maintainer guide](docs/COMMUNITY_LEARNING.md).

## Build and test

```bash
git clone https://github.com/Dizzybog/MatchLab.git
cd MatchLab
RUSTFLAGS="-D warnings" cargo test --workspace
docker build -t matchlab:local .
```

CI runs the Rust workspace tests and builds the production Docker image. Performance on a full IMDb dataset must still be checked on the target host.

## Credits and license

MatchLab is an independent project informed by these open-source projects:

- **[Comet](https://github.com/g0ldyy/comet)** — release parsing and airdate concepts.
- **[StremThru](https://github.com/MunifTanjim/stremthru)** — metadata ingestion and matching architecture.
- **[AIOStreams](https://github.com/Viren070/AIOStreams)** and its **[@aiostreams/release-parser](https://github.com/AIOStreams/release-parser)** (MIT) — comprehensive release-name handling, ordered parser handlers, match validators, season/episode safeguards, title cleanup, and file/folder metadata merging.
- **[TheBeastLT / parse-torrent-title](https://github.com/TheBeastLT/parse-torrent-title)** (MIT) — the Rust release-name parser used by [Torrentio](https://github.com/TheBeastLT/torrentio-scraper); reference for fractional anime-episode flags, Cyrillic episode-marker normalization, East Asian season notation and false-positive regression tests. MatchLab ports selected parsing ideas independently; it does not bundle the upstream crate or copy its runtime implementation.
- **[FTRL-Proximal](https://research.google.com/pubs/archive/41159.pdf)** — research basis for MatchLab's independent, native-Rust sparse online logistic learner (not a bundled third-party runtime).
- **[Rakun](https://github.com/lowlighter/rakun)** (MIT) — anime fansub naming conventions, `#episode` markers, multilingual labelled episode ranges, bracketed group/CRC metadata, and distinction between seasons, parts, volumes, and episodes.

The anime-specific improvements in this repository are **independently implemented in Rust**, not copied TypeScript code. See the [parser research and regression coverage](docs/ANIME_PARSER_RESEARCH.md) for the techniques studied, changes implemented, and intentionally excluded heuristics.

Original authors and their projects retain their respective licenses and credit. MatchLab is **not officially affiliated** with these projects or maintainers.

Licensed under **GPL-3.0-or-later**. External metadata sources have their own terms.
