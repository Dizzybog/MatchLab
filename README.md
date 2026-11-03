# Torrent Parse Match

**Torrent Parse Match** is a self-hosted service that identifies movies, TV shows and anime from torrent release names. It reads the complete filename, extracts details such as the title, year, season and episode, and returns the matching **IMDb ID**. For anime, it can also return an **AniDB ID** and the correct episode numbering.

For example, `Breaking.Bad.S02E03.1080p.WEB-DL` is recognized as *Breaking Bad*, season 2, episode 3.

The service provides a simple HTTP API and a web dashboard for testing matches. It runs in a single Docker container and stores its metadata in a local SQLite database.

## Features

- **Torrent title parsing:** Reads release names containing years, seasons, episodes, video quality, codecs and other release tags.
- **IMDb matching:** Searches movie and TV titles, including alternate titles and country-specific names. Uses exact-title, prefix and suffix matching, plus year and title-type ranking.
- **Anime matching:** Searches anime titles separately from IMDb. When an anime match has a valid episode mapping, it takes priority over ordinary IMDb candidates. Supports anime parts, alternate titles and conversion between AniDB and TVDB-style episode numbering.
- **Reliable results:** Returns one matching title or `null` when a reliable match cannot be established.
- **Local metadata:** Downloads IMDb and anime metadata, builds a SQLite database and checks for updates automatically.
- **Web dashboard:** Enter a release name and inspect the parsed information, IMDb candidates, anime candidate and selected result.
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

On its first start, the service downloads IMDb and anime metadata and creates its database. This can take some time, depending on your connection and hardware. The database is kept in the `metadata` Docker volume, so it survives container restarts and updates.

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

## Web dashboard

Open **http://127.0.0.1:8765/dashboard** in a browser on the machine running Docker.

Enter a torrent release name and select **Analyze**. The dashboard displays:

- The extracted title, year, season and episode.
- IMDb candidates and their matching titles.
- The anime candidate and its episode mapping, when available.
- The selected IMDb ID, season/episode ID (`SID`) and anime ID (`ASID`).
- Processing time and detailed JSON diagnostics.

You can optionally override the detected year or choose whether the release is a movie or TV series.

If Docker runs on another server, `127.0.0.1` refers to **that server**, not your own computer. Use an SSH tunnel or an authenticated HTTPS reverse proxy to access the dashboard remotely.

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

The service uses public IMDb datasets and anime title/episode mapping sources. Metadata is downloaded and indexed locally; release titles are not sent to an external matching API.

The service checks metadata on startup and every 24 hours. When the local data is at least seven days old, it attempts a refresh. If an update fails, the previous database remains available and the service retries later.

## Configuration

| Setting | Description |
| --- | --- |
| `LOG_LEVEL` | `INFO` for normal logs or `DEBUG` for detailed matching diagnostics. |
| `/data` | Persistent storage location for the SQLite metadata database. |
| `8765` | HTTP port for the API and dashboard. |

Logs include requests, matching decisions and metadata update activity. At `DEBUG` level, they contain detailed candidate information. Request titles and JSON payloads may appear in logs.

**Security:** The API and dashboard have no built-in authentication. The Compose example binds port 8765 to localhost. If you make the service available over a network, protect it with authentication and HTTPS.

## Build from source

```bash
git clone https://github.com/Dizzybog/torrent-parse-match.git
cd torrent-parse-match
docker build -t torrent-parse-match:local .
```

To run the Python tests, install Python 3.11+, Rust and the project dependencies:

```bash
python -m pip install . pytest
python -m pytest -q tests/
```

## License

GPL-3.0-or-later. Metadata sources have their own terms of use.
