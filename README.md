# Torrent Parse Match

**Torrent Parse Match** is a small, self-hosted service that identifies movies and TV shows from torrent release names.

Give it a name such as `Dune.2021.2160p.WEB-DL` or `Breaking.Bad.S02E03.1080p`. It extracts the title, year, season and episode, then searches a local database to find the corresponding **IMDb ID**. For anime, it can also return an **AniDB ID and episode number** when a reliable episode mapping exists.

You do not need to set up a database, download metadata manually, configure a scheduler, or know anything about other projects. Everything runs inside one Docker container.

## What it does

- Reads complete torrent release names, including quality and encoding tags.
- Matches movies and TV series to IMDb titles and alternate names.
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

## Current limitations

- Matching is deterministic; no AI model is required or included.
- Some uncommon anime episode mappings may not resolve.
- If several titles are equally plausible, the service may return `null` rather than guess.
- Real-world accuracy depends on metadata coverage and the release name.
- The first full metadata import and ARM64 deployment need verification with real data.

## License

GPL-3.0-or-later. Metadata providers have their own terms of use.
