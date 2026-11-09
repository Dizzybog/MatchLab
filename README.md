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
| `GET/POST /parse` | Extract release fields without matching |
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

**ML learns from every field that MatchLab parses**, including titles, years, seasons, episodes, languages, video and audio details, release groups, and other metadata. It looks for recurring naming patterns and can propose **new regex rules or improvements to existing ones** for any field. A rule is activated in the live parser **without restarting** only when sufficient independent evidence and field-specific safety checks support it; otherwise it remains inactive for further validation or review. Anime episode patterns still require verified metadata mappings. **ML changes regex only: MatchLab performs all field extraction and IMDb/AniDB matching.** With community sharing enabled, the exact regex additions and replacements can be proposed on GitHub. An ML score alone never activates a rule.

Enable learning in the Compose environment:

```yaml
environment:
  MATCHLAB_ML_ENABLED: "true"
  MATCHLAB_ML_TRAINING_MAX_EXAMPLES: "500000"
  MATCHLAB_ML_MAX_STORAGE_GB: "10"
  MATCHLAB_ML_CACHE_ENABLED: "true"
  MATCHLAB_ML_CACHE_MAX_ENTRIES: "200000"
```

Only `MATCHLAB_ML_ENABLED` is required; the other settings are optional limits. The training limit counts retained observations, **not RAM usage**. The local SQLite cache speeds up repeated requests, but after parser, rule or metadata changes, old answers are ignored and the next request is parsed and matched again. Learning evidence is never treated as a completed match.

Check `GET /api/ml/status` to see local model and rule statistics. No separate AI server, GPU or online AI service is required.

### Share improvements with the community (optional)

GitHub sharing is **off by default**, even when local ML is on. To contribute, create a GitHub token with permission to comment on this repository's Issues, store it privately, and set:

```yaml
environment:
  MATCHLAB_ML_ENABLED: "true"
  MATCHLAB_COMMUNITY_LEARNING_ENABLED: "true"
  MATCHLAB_COMMUNITY_GITHUB_TOKEN: ${MATCHLAB_COMMUNITY_GITHUB_TOKEN}
```

MatchLab can then submit opted-in learning reports and proposed regex changes for maintainer review. **Nothing is merged automatically.** Technical/TV/source-regex suggestions first enter a non-executable review queue; independently verified rules must be integrated before other users receive them. Reports can include sanitized release filenames, so only enable sharing if you accept publishing that information.

See [Machine Learning and Community Learning](docs/COMMUNITY_LEARNING.md) for supported patterns, limitations, privacy considerations, validation rules and contributor setup.

## Configuration and security

| Setting | Default | Description |
| --- | --- | --- |
| `MATCHLAB_DB` | `/data/metadata.sqlite3` | Local metadata database |
| `MATCHLAB_HOST` | `0.0.0.0:8765` in Docker | API listener |
| `MATCHLAB_METADATA_REFRESH_HOURS` | `168` | Metadata refresh interval |
| `MATCHLAB_MAX_CONCURRENT_MATCHES` | `4` | Maximum concurrent matches (example uses 2) |
| `MATCHLAB_ML_ENABLED` | `false` | Enable local background learning |
| `MATCHLAB_ML_TRAINING_MAX_EXAMPLES` | `12000` | Maximum retained training observations |
| `MATCHLAB_ML_MAX_STORAGE_GB` | `1` | Best-effort local learning and cache disk budget |
| `MATCHLAB_ML_CACHE_ENABLED` | `true` when ML is on | Cache repeat requests |
| `MATCHLAB_ML_CACHE_MAX_ENTRIES` | `100000` | Maximum persistent cache entries |
| `MATCHLAB_COMMUNITY_LEARNING_ENABLED` | `false` | Enable optional GitHub reporting |
| `MATCHLAB_COMMUNITY_GITHUB_TOKEN` | Unset | Token used only for opted-in sharing |

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
