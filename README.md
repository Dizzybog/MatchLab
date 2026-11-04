# Torrent Parse Match

**Torrent Parse Match** is a self-hosted service that identifies movies, TV shows and anime from torrent release names. It reads the complete filename, extracts details such as the title, year, season and episode, and returns the matching **IMDb ID**. For anime, it can also return an **AniDB ID** and the correct episode numbering.

For example, `Breaking.Bad.S02E03.1080p.WEB-DL` is recognized as *Breaking Bad*, season 2, episode 3.

The native Rust service provides an HTTP matching API backed by a local SQLite metadata database. The production image runs the native Rust service, including its metadata importer and optional scheduled refresh.

## Features

- **Torrent title parsing:** Reads release names containing years, seasons, episodes, video quality, codecs and other release tags.
- **IMDb matching:** Searches movie and TV titles, including alternate titles and country-specific names. Uses exact-title, prefix and suffix matching, plus year and title-type ranking.
- **Anime matching:** Searches anime titles separately from IMDb. When an anime match has a valid episode mapping, it takes priority over ordinary IMDb candidates. Supports anime parts, alternate titles and conversion between AniDB and TVDB-style episode numbering.
- **Reliable results:** Returns one matching title or `null` when a reliable match cannot be established.
- **Local metadata:** Reads an existing SQLite database. Automatic metadata downloads and refresh are available when six HTTPS metadata sources are configured.
- **Diagnostics:** Use `POST /debug/match` to inspect parsing, candidates and selection.
- **Docker support:** Runs on x86-64 and ARM64 machines, with persistent data and detailed logs.

This is a **metadata identification service**. It does not search for, download or stream torrents. It does not require an AI model.

## Getting started

You need Docker and Docker Compose installed.

Create a file called `compose.yaml`:

```yaml
services:
  torrent-parse-match:
    image: ghcr.io/dizzybog/torrent-parse-match:latest
    container_name: torrent-parse-match
    restart: unless-stopped
    environment:
      LOG_LEVEL: INFO
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

**Important:** Configure `MATCHLAB_METADATA_URLS` with six comma-separated HTTPS source URLs to initialize and periodically refresh metadata, or supply a populated compatible `/data/metadata.sqlite3`. Until a complete import succeeds, `/health` returns HTTP 503.

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

## Diagnostics

The Rust API exposes `POST /debug/match` for structured parsing and matching diagnostics. No dashboard is included in the production image.

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

Example response (selected fields):

```json
{
  "imdb_id": "tt1160419",
  "season": null,
  "episode": null,
  "sid": "tt1160419",
  "anime": null,
  "title": "Dune",
  "year": 2021,
  "category": "movie"
}
```

For TV episodes, `sid` uses the format `tt1234567:season:episode`. For anime with a verified episode mapping, `anime.asid` uses `anidb_id:episode`. Responses also include matching and parsing details.

The API returns JSON `null` when it cannot identify a reliable match. Invalid input returns HTTP `400`.

To inspect the candidate lists programmatically, use the diagnostic endpoint:

```bash
curl -X POST http://127.0.0.1:8765/debug/match \
  -H "Content-Type: application/json" \
  -d '{"title":"Breaking.Bad.S02E03.1080p"}'
```

### Matching process

1. The parser extracts the title and release information.
2. The service searches IMDb metadata for movie and TV candidates.
3. For episodic releases, it also searches anime metadata independently.
4. A verified anime match takes priority; otherwise, the best eligible IMDb candidate is considered.
5. The service returns the selected ID and episode information, or `null` if the match is uncertain.

Anime episode conversion depends on the available mappings. Some unusual titles or episode-numbering formats may not resolve.

## Metadata and updates

The native Rust backend supports `refresh-metadata` for six local metadata files and `refresh-metadata-urls` for six HTTPS sources (IMDb basics, AKAs, episodes, animeApi, Anime-Lists, AniDB titles). To enable automatic startup initialization and periodic refresh, set `MATCHLAB_METADATA_URLS` to six comma-separated HTTPS URLs. `MATCHLAB_METADATA_REFRESH_HOURS` defaults to 24 and accepts values from 1 to 720. Each complete refresh is validated in a staging SQLite database before atomic publication; failures preserve the existing database.

## Configuration

| Setting | Description |
| --- | --- |
| `RUST_LOG` | Rust tracing filter, e.g. `matchlab_server=info`. |
| `/data` | Persistent storage location for the SQLite metadata database. |
| `MATCHLAB_DB` | SQLite path (default `/data/metadata.sqlite3`). |
| `MATCHLAB_HOST` | HTTP listen address (Docker default `0.0.0.0:8765`). |
| `MATCHLAB_METADATA_URLS` | Six comma-separated HTTPS source URLs; enables automatic refresh. |
| `MATCHLAB_METADATA_REFRESH_HOURS` | Refresh interval, default 24 hours, allowed 1–720. |
| `8765` | HTTP port for the Rust API. |

Logs include Rust service and matching activity. At `DEBUG` level, they contain detailed candidate information. Request titles and JSON payloads may appear in logs.

**Security:** The API and dashboard have no built-in authentication. The Compose example binds port 8765 to localhost. If you make the service available over a network, protect it with authentication and HTTPS.

## Build from source

```bash
git clone https://github.com/Dizzybog/torrent-parse-match.git
cd torrent-parse-match
docker build -t torrent-parse-match:local .
```

Run the native Rust tests:

```bash
RUSTFLAGS='-D warnings' cargo test -p matchlab-server
```

## License

GPL-3.0-or-later. Metadata sources have their own terms of use.
