# MatchLab

**MatchLab** is a self-hosted service that identifies movies, TV shows and anime from torrent release names. It reads the complete filename, extracts details such as the title, year, season and episode, and returns the matching **IMDb ID**. For anime, it can also return an **AniDB ID** and the correct episode numbering.

For example, `Breaking.Bad.S02E03.1080p.WEB-DL` is recognized as *Breaking Bad*, season 2, episode 3.

The native Rust service provides an HTTP matching API backed by a persistent Tantivy title index. SQLite remains the source for relational metadata such as ratings, seasons and anime episode crosswalks. The production image includes automatic native import and scheduled refresh.

## Features

- **Torrent title parsing:** Reads release names containing years, seasons, episodes, video quality, codecs and other release tags.
- **IMDb matching:** Searches movie and TV titles, including alternate titles and country-specific names. Uses exact-title, prefix and suffix matching, plus year and title-type ranking.
- **Anime matching:** Resolves anime titles and aliases using AniDB, animeApi and Anime-Lists crosswalks. Verified episode mappings can take priority over IMDb ranking, while exact unrelated IMDb TV-series titles are protected from anime alias collisions. Supports anime parts, absolute episode numbering and ranges that cross mapped seasons.
- **Hierarchical file paths:** Reads a specific file or episode from the final path component and can inherit a missing series title from parent folders. Parent season-pack ranges do not override the file's own season/episode.
- **Absolute episode ranges:** Preserves both endpoints (e.g. `E768-E1064`) and resolves episodes individually across mapped TVDB-style seasons, reporting unmapped gaps instead of guessing.
- **Reliable results:** Returns one matching title or `null` when a reliable match cannot be established.
- **Local metadata:** Maintains a validated SQLite metadata snapshot and immutable on-disk Tantivy index generations. Downloads seven public sources and refreshes them on a schedule.
- **Diagnostics:** Use `POST /debug/match` to inspect parsing, candidates and selection.
- **Docker support:** Runs on x86-64 and ARM64 machines, with persistent data and detailed logs.

This is a **metadata identification service**. It does not search for, download or stream torrents. It does not require an AI model.

## Getting started

You need Docker and Docker Compose installed.

Create a file called `compose.yaml`:

```yaml
services:
  matchlab:
    image: ghcr.io/dizzybog/matchlab:latest
    container_name: matchlab
    restart: unless-stopped
    environment:
      RUST_LOG: matchlab_server=info
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

Start the service:

```bash
docker compose up -d
```

**No extra environment variables are required.** On a fresh volume the Rust API starts and downloads seven public metadata feeds in the background. `/health` returns HTTP 503 until the first complete import succeeds; this requires outbound HTTPS access, disk space and time. Later refresh failures preserve the existing database.

Check the logs:

```bash
docker compose logs -f
```

To update the application later:

```bash
docker compose pull
docker compose up -d
```

The Docker image is published from GitHub Actions. If the image is not publicly accessible yet, you may need to build it from source or authenticate with GitHub Container Registry.

## Tantivy index and safe metadata refresh

IMDb and AniDB title discovery use Tantivy for exact aliases, country-qualified titles, prefixes, reversed suffixes, shorter-title fallback and duplicate-title detection. The index stores IMDb alias metadata directly so the matching pipeline does not perform SQLite title-table searches. SQLite is used only for relational season, episode, votes and anime crosswalk lookups.

A metadata refresh validates a staging SQLite database, builds and verifies its new Tantivy generation under `/data/metadata.sqlite3.tantivy/`, and publishes the SQLite database only after the index is complete. A generation marker inside SQLite ensures in-flight requests retain the correct old index. A normal restart reopens the existing index; a legacy SQLite database without generation metadata is indexed once during startup.

Allow space for overlapping metadata and index generations during an import. Historical completed indexes are retained conservatively before cleanup. The common exact-title path uses indexed term equality to avoid expensive broad prefix scans.

## Dashboard and diagnostics

The Rust API exposes `POST /debug/match` for structured parsing and matching diagnostics. The production service also serves the MatchLab dashboard at `/`, with request statistics and history. The dashboard presents up to five ranked candidates as readable cards, showing IMDb ID, title, year, media type, season/episode and available AniDB details. The selected match is highlighted. The raw JSON diagnostics remain available through the API.

## API

The API listens on port **8765**.

### Check status

```bash
curl http://127.0.0.1:8765/health
```

The service returns HTTP `200` when the metadata is ready and `503` while it is initializing.

### Minimal matching API — `/match`

`GET /match` and `POST /match` return **exactly four fields**: `sid_start`, `sid_end`, `asid_start` and `asid_end`. The matcher runs independently: you **do not** need to call `/parse` first.

```bash
curl -X POST http://127.0.0.1:8765/match \
  -H "Content-Type: application/json" \
  -d '{"title":"Supernatural.S01E01-E22.1080p"}'
```

Example response (assuming the selected IMDb identity is `tt0460681`):

```json
{
  "sid_start": "tt0460681:1:1",
  "sid_end": "tt0460681:1:22",
  "asid_start": null,
  "asid_end": null
}
```

- **Movie:** both SID fields contain the IMDb title ID, e.g. `tt1160419`.
- **Single TV episode:** both SID fields contain `ttID:season:episode`.
- **Episode/season pack:** SID fields identify the first and last episode when verified endpoints are available. Missing episode metadata is not guessed.
- **Anime:** ASID fields contain `anidb_id:episode` when verified mappings exist. An interval may cross AniDB IDs; the start and end can therefore have different prefixes.
- **No reliable match:** all four fields are `null`. Missing individual mappings are `null`; no synthetic IDs are returned.

For a URL query, use `GET /match?title=Supernatural.S01E01`. Both endpoints accept `title` or `t_title` as the request input. A supplied `year` can constrain matching.

### Full parser API — `/parse`

`GET /parse` and `POST /parse` return the **complete structured parser output** for the torrent title, without requiring an IMDb/AniDB database lookup.

```bash
curl -X POST http://127.0.0.1:8765/parse \
  -H "Content-Type: application/json" \
  -d '{"title":"Supernatural.S02E03.1080p.WEB-DL"}'
```

The JSON contains every field the parser recognizes, such as `title`, `seasons`, `episodes`, `year`, `resolution`, `quality`, `codec`, `languages` and release metadata. Fields not recognized in the input are omitted; the exact set depends on the title. `GET /parse?title=...` is also supported.

### Airdate matching (no API key)

When a TV release contains a date instead of explicit SxxExx coordinates, MatchLab
identifies the IMDb series from the title preceding the date, then looks up
the episode's premiere date in Cinemeta's public episode feed.

```text
The.Daily.Show.2022.01.20.Francis.Ngannou.1080p
```

```bash
curl 'http://127.0.0.1:8765/match?title=The.Daily.Show.2022.01.20.Francis.Ngannou.1080p'
```

On a verified date match, the normal four-field response contains
`ttID:season:episode` in both SID fields. Explicit SxxExx information
takes precedence over a date. Cinemeta is accessed only when needed, and
its episode list is cached for seven days in `/data/matchlab_airdates.sqlite3`.
No API key is required. If the upstream feed fails or the date is missing
or ambiguous, MatchLab does not invent episode coordinates. The existing
IMDb metadata database is never modified by these lookups.

### Shared 30-second cache

Both APIs use the same native Rust parser. A bounded, in-memory cache stores parsed results by **exact input title** for **30 seconds**. If a client calls `/parse` and then `/match` with the same title, the second request can reuse the parsing result; the reverse order works too. `/match` always performs its own metadata matching and does not require a preceding parse call.

The cache is process-local (not shared between replicas), has a maximum of 1,024 entries, and does not persist across restarts. The cache stores parsing results, **not** full match results.

For detailed ranking, candidates, episode mapping segments and debugging, use `POST /debug/match`. The dashboard at `/` also uses diagnostic data.

### Hierarchical paths and folder inheritance

Pass a full relative path as the `title` or `t_title` value. Forward slashes and backslashes separate path components. MatchLab parses the **leaf** (last component) first. If the leaf supplies season/episode coordinates but lacks a meaningful series title, it looks for a title in the nearest suitable parent folder. Folder names supply missing context, **not** a replacement for the leaf's explicit episode information.

| Input | Intended interpretation |
| --- | --- |
| `Supernatural S01-S12/S02E01` | Series `Supernatural`, season 2, episode 1; `S01-S12` is collection context. |
| `Supernatural S01-S12/S02E01.mkv` | Same episode, with a filename extension. |
| `One.piece.E768-E1064` | Absolute episode interval 768–1064 (297 episodes), potentially crossing season boundaries. |

A parent folder does not automatically assign its season range to a child file. Explicit child coordinates take precedence. This is title inheritance, not a filesystem crawler: MatchLab parses the path **string supplied in the request** and does not inspect directories on disk.

### Anime identity and episode mapping

Anime matching uses the same local metadata snapshot as IMDb matching, supplemented by **AniDB titles**, **animeApi** identity links and **Anime-Lists** episode mappings. It does not rely on AI inference or fabricate an IMDb/AniDB relationship.

1. The Rust parser extracts a title, optional season/episode and any absolute episode interval.
2. IMDb matching searches original and alternate titles (including country-specific aliases) and ranks eligible movie or TV-series identities.
3. Anime title matching independently checks AniDB aliases and linked IMDb identities. Exact anime aliases and anime-part titles can be relevant even when the release lacks explicit season tokens.
4. Where an episode is supplied, the resolver uses the imported crosswalk tables to translate between absolute anime numbering, AniDB numbering and mapped TVDB-style season/episode coordinates. Explicit mappings and split-season rules are respected.
5. A verified anime match can outrank an ordinary IMDb title candidate. An unrelated anime sharing an alias must not override an exact IMDb TV-series title.
6. If a required episode mapping is missing or ambiguous, the service does not invent canonical episode coordinates. The selected IMDb identity may still be available without a verified anime episode.

**Important:** Episode mappings in this project are **TVDB-style** coordinates from Anime-Lists, not independently verified IMDb season assignments. Do not assume the returned season number always equals IMDb's own season numbering.

#### Absolute episode intervals across seasons

For `One.piece.E768-E1064`, the parser preserves:

```json
{
  "episode_start": 768,
  "episode_end": 1064,
  "episode_count": 297
}
```

The diagnostic endpoint (`POST /debug/match`) resolves each episode independently. Its `episode_range` object contains `absolute_start`, `absolute_end`, `start_season`, `start_episode`, `end_season`, `end_episode`, `segments`, `unmapped_count` and `complete`. Each segment groups consecutive, verified episodes within a mapped season. A range can span multiple seasons. `complete` is `true` only if every episode has a mapping; missing endpoints remain `null`, and unmapped episodes are counted rather than interpolated. The public `/match` response exposes only `sid_start`, `sid_end`, `asid_start` and `asid_end`; full segment details remain in diagnostics.

The availability and correctness of conversions depend on the imported metadata and its season conventions.

When no reliable match exists, `/match` returns an object with four `null` values. Invalid input returns HTTP `400`.

To inspect the candidate lists programmatically, use the diagnostic endpoint:

```bash
curl -X POST http://127.0.0.1:8765/debug/match \
  -H "Content-Type: application/json" \
  -d '{"title":"Breaking.Bad.S02E03.1080p"}'
```

### Complete season packs

MatchLab imports IMDb `title.episode.tsv.gz` into a per-season `season_episodes` table, keyed by IMDb series ID and season number. Each row stores `episode_count` (number of numbered episode rows) and `max_episode` (highest observed episode number). The matcher uses **max_episode**, because episode numbering may have gaps.

For a season-only release such as `Supernatural.S02`, the selected match receives `season=2`, `episode=1`, `season_end=2`, and `episode_end=22` **if** the imported IMDb metadata reports episode 22 as the maximum. A concrete episode such as `Supernatural.S02E01` remains a single episode and is not expanded. When season metadata is missing, MatchLab does not invent an endpoint.

The season table uses IMDb season numbering; anime crosswalk mappings may use different TVDB-style season conventions.

## Metadata and updates

The native Rust backend supports `refresh-metadata` for six local metadata files and `refresh-metadata-urls` for seven HTTPS sources (IMDb basics, AKAs, episodes, ratings, animeApi, Anime-Lists, AniDB titles). Automatic initialization and periodic refresh use seven built-in public HTTPS URLs. `MATCHLAB_METADATA_URLS` accepts seven URLs, or the legacy six-URL configuration (the official IMDb ratings URL is inserted automatically). `MATCHLAB_METADATA_REFRESH_HOURS` defaults to 168 and accepts values from 1 to 720. Each complete refresh is validated in a staging SQLite database before atomic publication; failures preserve the existing database.

## Configuration

| Setting | Description |
| --- | --- |
| `RUST_LOG` | Rust tracing filter, e.g. `matchlab_server=info`. |
| `/data` | Persistent storage location for the SQLite metadata database. |
| `MATCHLAB_DB` | SQLite path (default `/data/metadata.sqlite3`). |
| `MATCHLAB_HOST` | HTTP listen address (Docker default `0.0.0.0:8765`). |
| `MATCHLAB_METADATA_URLS` | Optional override of seven HTTPS metadata source URLs; legacy six-URL configuration is accepted. |
| `MATCHLAB_METADATA_REFRESH_HOURS` | Refresh interval, default 168 hours, allowed 1–720. |
| `8765` | HTTP port for the Rust API. |

Logs include Rust service and matching activity. At `DEBUG` level, they contain detailed candidate information. Request titles and JSON payloads may appear in logs.

**Security:** The API has no built-in authentication. The Compose example binds port 8765 to localhost. If you make the service available over a network, protect it with authentication and HTTPS.

## Build from source

```bash
git clone https://github.com/Dizzybog/MatchLab.git
cd matchlab
docker build -t matchlab:local .
```

Run the native Rust tests:

```bash
RUSTFLAGS='-D warnings' cargo test -p matchlab-server
```

## Acknowledgements and upstream projects

MatchLab builds on ideas and work from the open-source community. Special thanks to:

- **[Comet by g0ldyy](https://github.com/g0ldyy/comet)** for the torrent release-name parser foundations and the approach to matching date-based TV releases to season and episode numbers. Comet's [episode index implementation](https://github.com/g0ldyy/comet/blob/main/comet/metadata/episode_index.py) inspired MatchLab's native Rust Cinemeta airdate resolver and episode cache. MatchLab implements these capabilities in Rust, without a Python backend.
- **[StremThru by MunifTanjim](https://github.com/MunifTanjim/stremthru)** for its metadata collection and ingestion approach, which informed MatchLab's native metadata import pipeline.

Thank you to the maintainers and contributors of both projects. MatchLab is an independent project and is not officially affiliated with either team.

## License

GPL-3.0-or-later. Metadata sources have their own terms of use.
