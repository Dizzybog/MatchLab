# MatchLab

**MatchLab** is a self-hosted service that identifies movies, TV shows and anime from torrent release names. It reads the complete filename, extracts details such as the title, year, season and episode, and returns the matching **IMDb ID**. For anime, it can also return an **AniDB ID** and the correct episode numbering.

For example, `Breaking.Bad.S02E03.1080p.WEB-DL` is recognized as *Breaking Bad*, season 2, episode 3.

The native Rust service provides an HTTP matching API backed by a local SQLite metadata database. The production image runs the native Rust service, including its metadata importer and automatic scheduled refresh.

## Features

- **Torrent title parsing:** Reads release names containing years, seasons, episodes, video quality, codecs and other release tags.
- **IMDb matching:** Searches movie and TV titles, including alternate titles and country-specific names. Uses exact-title, prefix and suffix matching, plus year and title-type ranking.
- **Anime matching:** Resolves anime titles and aliases using AniDB, animeApi and Anime-Lists crosswalks. Verified episode mappings can take priority over IMDb ranking, while exact unrelated IMDb TV-series titles are protected from anime alias collisions. Supports anime parts, absolute episode numbering and ranges that cross mapped seasons.
- **Hierarchical file paths:** Reads a specific file or episode from the final path component and can inherit a missing series title from parent folders. Parent season-pack ranges do not override the file's own season/episode.
- **Absolute episode ranges:** Preserves both endpoints (e.g. `E768-E1064`) and resolves episodes individually across mapped TVDB-style seasons, reporting unmapped gaps instead of guessing.
- **Reliable results:** Returns one matching title or `null` when a reliable match cannot be established.
- **Local metadata:** Reads an existing SQLite database. Automatically downloads six public metadata sources on first startup and refreshes them periodically; custom URLs are optional.
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

**No extra environment variables are required.** On a fresh volume the Rust API starts and downloads six public metadata feeds in the background. `/health` returns HTTP 503 until the first complete import succeeds; this requires outbound HTTPS access, disk space and time. Later refresh failures preserve the existing database.

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

## Dashboard and diagnostics

The Rust API exposes `POST /debug/match` for structured parsing and matching diagnostics. The production service also serves the MatchLab dashboard at `/`, with request statistics and history. The dashboard presents up to five ranked candidates as readable cards, showing IMDb ID, title, year, media type, season/episode and available AniDB details. The selected match is highlighted. The raw JSON diagnostics remain available through the API.

## API

The API listens on port **8765**.

### Check status

```bash
curl http://127.0.0.1:8765/health
```

The service returns HTTP `200` when the metadata is ready and `503` while it is initializing.

### Match a title

```bash
curl -X POST http://127.0.0.1:8765/match \
  -H "Content-Type: application/json" \
  -d '{"t_title":"Dune.2021.2160p.WEB-DL"}'
```

Example response (selected fields; the public endpoint uses `imdbid`, not `imdb_id`):

```json
{
  "imdbid": "tt1160419",
  "season": null,
  "episode": null,
  "sid": "tt1160419",
  "anime_id": null,
  "asid": null,
  "type": "movie",
  "parsed": {
    "title": "Dune",
    "year": 2021
  }
}
```

For TV episodes, `sid` uses `tt1234567:season:episode` when the season and episode are known. For anime with verified AniDB episode coordinates, `asid` uses `anidb_id:episode`. `parsed` contains the parser output (which can include more fields than shown here). `GET /match?title=...` is also supported. The full selected record and ranked IMDb candidates are available through `POST /debug/match`.

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

The matcher resolves each episode independently. The `episode_range` response contains `absolute_start`, `absolute_end`, `start_season`, `start_episode`, `end_season`, `end_episode`, `segments`, `unmapped_count` and `complete`. Each segment groups consecutive, verified episodes within a mapped season. A range can span multiple seasons. `complete` is `true` only if every episode has a mapping; missing endpoints remain `null`, and unmapped episodes are counted rather than interpolated. Top-level `season_end` and `episode_end` are also exposed. The ordinary `sid` refers to the mapped start episode when available; it is **not** a representation of the entire range.

The availability and correctness of conversions depend on the imported metadata and its season conventions.

The API returns JSON `null` when it cannot identify a reliable match. Invalid input returns HTTP `400`.

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

The native Rust backend supports `refresh-metadata` for six local metadata files and `refresh-metadata-urls` for six HTTPS sources (IMDb basics, AKAs, episodes, animeApi, Anime-Lists, AniDB titles). Automatic initialization and periodic refresh use six built-in public HTTPS URLs. `MATCHLAB_METADATA_URLS` can optionally override all six URLs. `MATCHLAB_METADATA_REFRESH_HOURS` defaults to 168 and accepts values from 1 to 720. Each complete refresh is validated in a staging SQLite database before atomic publication; failures preserve the existing database.

## Configuration

| Setting | Description |
| --- | --- |
| `RUST_LOG` | Rust tracing filter, e.g. `matchlab_server=info`. |
| `/data` | Persistent storage location for the SQLite metadata database. |
| `MATCHLAB_DB` | SQLite path (default `/data/metadata.sqlite3`). |
| `MATCHLAB_HOST` | HTTP listen address (Docker default `0.0.0.0:8765`). |
| `MATCHLAB_METADATA_URLS` | Optional override of six built-in HTTPS metadata source URLs. |
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

## License

GPL-3.0-or-later. Metadata sources have their own terms of use.
