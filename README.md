# Torrent Parse Match

**Torrent Parse Match** is a small, self-hosted service that identifies movies and TV shows from torrent release names.

Give it a name such as `Dune.2021.2160p.WEB-DL` or `Breaking.Bad.S02E03.1080p`. It extracts the title, year, season and episode, then searches a local database to find the corresponding **IMDb ID**. For anime, it can also return an **AniDB ID and episode number** when a reliable episode mapping exists.

You do not need to set up a database, download metadata manually, configure a scheduler, or know anything about other projects. Everything runs inside one Docker container.

## What it does

- Reads complete torrent release names, including quality and encoding tags.
- Matches movies and TV series to IMDb titles and alternate names using ordered exact, prefix, reverse-suffix and half-prefix matching stages.
- Recognizes release years and season/episode numbers.
- Supports anime aliases and conversions between different episode-numbering systems.
- Returns **one match** in JSON, or `null` if it cannot determine a reliable match.
- Downloads the metadata it needs and updates it automatically.
- Runs on standard x86-64 and ARM64 Docker hosts, including compatible cloud servers.

This is a **metadata matching API**, not a torrent downloader, torrent search engine or streaming service. It does not download torrents.

## Install with Docker

You need Docker installed and an Internet connection.

### Option A: Docker Compose (recommended)

Create a file named `compose.yaml`:

```yaml
services:
  torrent-parse-match:
    image: ghcr.io/dizzybog/torrent-parse-match:latest
    pull_policy: always
    environment:
      LOG_LEVEL: DEBUG
    container_name: torrent-parse-match
    restart: unless-stopped
    ports:
      - "127.0.0.1:8765:8765"
    volumes:
      - metadata:/data

volumes:
  metadata:
```

Start it:

```bash
docker compose up -d
```

To update the application image later:

```bash
docker compose pull
docker compose up -d
```

### Option B: Docker commands

```bash
docker pull ghcr.io/dizzybog/torrent-parse-match:latest
docker volume create torrent-parse-match-data
docker run -d \
  --name torrent-parse-match \
  --restart unless-stopped \
  -p 127.0.0.1:8765:8765 \
  -v torrent-parse-match-data:/data \
  ghcr.io/dizzybog/torrent-parse-match:latest
```

**Note:** The image is published by the repository's GitHub Actions workflow when changes reach `main`. It will be pullable after that workflow succeeds and the GitHub package is made public. If the package is private, GitHub authentication is required.

### First start

The container downloads IMDb and anime metadata and builds its own SQLite database. The first download can take a while and needs free disk space. Follow progress with:

```bash
docker logs -f torrent-parse-match
```

The database is stored in the Docker volume, so restarting or updating the container does not delete it.

### Logs and shutdown

The container uses **tini** as PID 1 to forward signals and reap child processes. The API handles SIGTERM/SIGINT and stops accepting requests cleanly.

By default, `LOG_LEVEL=DEBUG` prints timestamps and detailed diagnostics to standard output. View them with:

```bash
docker logs -f --timestamps torrent-parse-match
```

Logs include incoming HTTP method, path, remote IP and request body, each response status and JSON body, title parsing, IMDb candidate/matching stage, metadata freshness checks, download URLs and HTTP status, import counts, errors with stack traces, and startup/shutdown. Metadata checks run at startup and then daily; downloads occur when data is missing or at least seven days old. Set `LOG_LEVEL=INFO` to reduce per-candidate debug output.

**Privacy:** Request titles and complete JSON request/response bodies appear in logs. Avoid putting secrets in request bodies or query strings, and restrict access to Docker logs. Metadata downloads are logged by URL and HTTP status; large metadata file contents are not logged.

## Use the API

The service listens on port **8765**.

### Check whether it is ready

```bash
curl -i http://127.0.0.1:8765/health
```

Before the first metadata download finishes, the service returns HTTP `503`. After the database is ready, it returns HTTP `200`.

### Match a torrent title

```bash
curl -X POST http://127.0.0.1:8765/match \
  -H "Content-Type: application/json" \
  -d '{"t_title":"Dune.2021.2160p.WEB-DL"}'
```

A match contains fields like:

```json
{
  "imdb_id": "tt1160419",
  "season": null,
  "episode": null,
  "sid": "tt1160419",
  "anime": null,
  "title": "Dune",
  "year": 2021,
  "category": "movie",
  "matched_title": "Dune"
}
```

The actual response also includes parsed title information and available anime IDs. If a series episode is recognized, `sid` uses the format `tt1234567:season:episode`. When anime episode mapping succeeds, `anime.asid` uses the format `AniDB_ID:episode`.

You can also use:

```bash
curl -G http://127.0.0.1:8765/match \
  --data-urlencode "title=Breaking.Bad.S02E03.1080p"
```

An unknown or ambiguous title returns JSON `null`. Invalid requests return HTTP `400`.

## Automatic metadata updates

The container checks the last successful update:

1. When it starts.
2. Every 24 hours while running.
3. If the data is **7 days old or older**, it downloads fresh metadata.
4. If updating fails, it keeps the previous working database and retries after one hour.

You do not need to set up cron or manually update any tables.

Metadata comes from IMDb's publicly available datasets and public anime title/episode mapping sources. Only IMDb entries with supported movie or TV-series types are stored. Unsupported entries such as video games and individual episode titles are excluded to reduce storage.

## Privacy and security

All matching happens locally against the downloaded database. The container needs Internet access for metadata updates but does not send each title to an external matching API.

The included Docker configuration only exposes the API on `127.0.0.1` (your own server). **The API has no built-in authentication.** Use an authenticated HTTPS reverse proxy before making it publicly accessible.

## Build from source (optional)

```bash
git clone https://github.com/Dizzybog/torrent-parse-match.git
cd torrent-parse-match
docker build -t torrent-parse-match:local .
```

To run the tests locally, install Python 3.11+, Rust and the package dependencies, then run:

```bash
python -m pip install . pytest
python -m pytest -q tests/
```

## Matching behavior

The IMDb matcher uses ordered matching stages inspired by its PostgreSQL V4 implementation: forward/exact prefix, shorter prefix, indexed reverse suffix, and half-prefix fallback. Candidate selection also uses year proximity, original title, US aliases, title type and alias counts. Season limits are checked for TV series. Matching runs entirely against local SQLite metadata; no PostgreSQL instance is needed.

The implementation is not yet a byte-for-byte port of every PostgreSQL V4 branch. In particular, country-specific alias expansion and some existing-ID validation rules still differ. The source has regression tests for the core prefix and suffix cases.

## Current limitations

- Matching is deterministic; no AI model is required or included.
- Some uncommon anime episode mappings may not resolve.
- If several titles are equally plausible, the service may return `null` rather than guess.
- Real-world accuracy depends on metadata coverage and the release name.
- The first full metadata import and ARM64 deployment need verification with real data.

## License

GPL-3.0-or-later. Metadata providers have their own terms of use.


### Country-qualified IMDb titles and anime part titles

The V4 IMDb matcher includes a small built-in country-alias table
(`python/MediaMatch/country_aliases.py`). An explicit trailing country
suffix in a TV-series release (e.g. `The.Office.UK.S01E01`) is expanded
to equivalent IMDb AKA names before the generic prefix stage. Only actual
IMDb AKA rows are eligible; the country dictionary does not invent IDs.

For anime, `python/MediaMatch/anime_title_matcher.py` uses local AniDB
title aliases and the StremThru-derived `anime_series_map` and
`episode_map` SQLite tables. A generic series title treats `SxxExx`
as a global TVDB season; a specific arc/part title interprets `S01`
relative to that AniDB part. Responses return the mapped canonical
IMDb/TVDB season/episode as well as the AniDB `asid`. If a specific
part is recognized but its requested episode cannot be mapped, the
API returns no match rather than silently using an unrelated global
season. Detailed matching stages appear at `LOG_LEVEL=DEBUG`.

This is a targeted port of the older `torrent_stream` anime resolver.
Exact parity for all split Trakt seasons and exceptional Anime-Lists
mapping expressions still requires fixture-based verification.


### Independent IMDb and anime candidate pools

The matcher searches both metadata sources for every eligible episodic
release. IMDb V4 title matching supplies ordinary movie/TV candidates.
Separately, AniDB aliases and animeApi/Anime-Lists supply anime candidates,
including their IMDb IDs and verified season/episode crosswalks.

When a unique anime candidate has a valid episode mapping, it takes
priority over ordinary IMDb candidates (including similarly named films).
If anime mapping is missing or ambiguous, that anime candidate is
rejected and the best valid IMDb candidate may be selected instead.
Anime-derived IDs do not require an IMDb title/AKA row.

Logs identify both candidate pools and the selected source as
`match_source: "anime_priority"` for anime. The anime candidate's
`title` and `year` fields are parsed from the release, not verified
against IMDb title metadata.

### MatchLab web dashboard

Open **http://127.0.0.1:8765/dashboard** after the service is running and its initial metadata download has finished. The dashboard is included in the existing Docker image and uses the same HTTP port; no additional container or database is required.

Paste a torrent release title, optionally override its year or type, and click **Analyze**. The dashboard shows parsed fields, IMDb V4 candidates, anime candidate and canonical episode mapping, selected IMDb ID/SID/ASID, reason for selection, execution time and expandable JSON diagnostics. The same diagnostics are available via `POST /debug/match` with `{"title":"Example.Show.S01E01"}`.

The dashboard and diagnostic endpoint expose metadata and log complete request titles. Keep port 8765 bound to localhost or protect it with authentication and HTTPS before exposing it publicly. The provided Compose configuration binds to `127.0.0.1` by default.
