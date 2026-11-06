# MatchLab

**MatchLab** is a self-hosted Rust service that recognizes movies, TV series, and anime from torrent filenames. Send a release name to its HTTP API to get an **IMDb ID**, verified episode coordinates, and—when available—an **AniDB ID**.

It can parse names such as `Breaking.Bad.S02E03.1080p.WEB-DL`, recognize season packs, and handle anime episode mapping. It **does not** search for or download torrents.

## Features

- **Fast title matching:** Tantivy indexes IMDb titles, alternate titles, and AniDB aliases. MatchLab ranks exact titles, country aliases, year, and media type using deterministic rules.
- **Anime support:** AniDB, animeApi, and Anime-Lists provide verified identities and episode conversions, including split seasons and absolute episode numbering.
- **Full release parsing:** Extracts title, year, season, episodes, resolution, languages, release tags, and more. Supports filenames inside folder paths.
- **Easy deployment:** Native Rust server with Docker, an HTTP API, a dashboard, and automatic metadata refresh.
- **Safe updates:** SQLite stores metadata relationships; Tantivy generations are built and validated before metadata is switched.

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

On first startup, MatchLab downloads **seven public metadata feeds** and builds its local SQLite database and Tantivy index. This requires outbound HTTPS access and sufficient free disk space. `/health` returns **503** while initialization is in progress and **200** when ready. Later metadata refreshes preserve the last published data if an import fails.

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
| `MATCHLAB_METADATA_URLS` | Built-in HTTPS feeds | Optional seven-source override; legacy six-source lists are supported |
| `RUST_LOG` | `matchlab_server=info` | Logging verbosity |

**History and statistics retention:** The dashboard keeps at most **1,000 latest individual requests** (including full diagnostics), regardless of age. Its **Requests, Matched, Unmatched, Errors, and average latency** statistics cover all requests from the **last 14 days (UTC, minute precision)**, not just those 1,000 rows. A separate SQLite `request_minute_stats` table accumulates counters by minute; the dashboard activity chart groups them into daily totals. During the first startup after upgrading, the last 14 days are backfilled from any existing legacy request history **before** old request rows are removed. An indexed SQLite trigger evicts only the oldest request detail row on each insert once the 1,000-row cap is reached; it never alters aggregate statistics. A background maintenance task additionally runs at startup and every five minutes to remove bulk legacy history and expire aggregate buckets older than 14 days, off the asynchronous HTTP executor. This does not modify IMDb metadata. SQLite generally reuses freed pages instead of immediately shrinking the existing file; run `VACUUM` during a maintenance window once after upgrading if reclaiming disk space is important. Back up a live SQLite database using a consistent backup method, not a direct file copy.

For importer options and the exact source URLs, see [Metadata lifecycle](docs/LOCAL_METADATA.md). For index internals and crash-safety considerations, see [Tantivy architecture](docs/TANTIVY_RUNTIME.md).

**Security:** There is no built-in authentication. This example binds the port to **localhost**; put the service behind an authenticated HTTPS reverse proxy before exposing it over a network. Debug logs may contain the provided filenames.

## Build and test

```bash
git clone https://github.com/Dizzybog/MatchLab.git
cd MatchLab
RUSTFLAGS="-D warnings" cargo test --workspace
docker build -t matchlab:local .
```

CI runs the Rust workspace tests and builds the production Docker image. Performance on a full IMDb dataset must still be checked on the target host.

## Credits and license

MatchLab is an independent project informed by [Comet](https://github.com/g0ldyy/comet) for release parsing/airdate concepts and [StremThru](https://github.com/MunifTanjim/stremthru) for metadata ingestion and matching architecture. Their original projects and licenses remain credited; MatchLab is not officially affiliated with either team.

Licensed under **GPL-3.0-or-later**. External metadata sources have their own terms.
