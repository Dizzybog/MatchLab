# Torrent Parse Match

Self-hosted torrent title parsing and IMDb/AniDB matching API. Built on a high-performance Rust implementation of PTT and RTN, with Python orchestration and a local SQLite metadata index. The repository also retains the original parsing/ranking interfaces.

**Status:** IMDb/AniDB matching is an evolving implementation, not a complete replacement for the PostgreSQL IMDb V4 matcher. Automated fixture tests and an x86_64 Docker build have passed. Full upstream data refresh, accuracy on production IMDb records, and Oracle ARM64 deployment still require validation.

## Features

- Accept a **raw torrent release name** (`t_title`); RTN extracts the title, year, season, episode, quality and other metadata.
- Search a local SQLite IMDb index; return **one match object or JSON `null`**, not a candidate list.
- Preserve IMDb season/episode in `sid` and, when a verified anime mapping exists, AniDB episode in `anime.asid`.
- Use IMDb AKA titles, year, media type, duplicate-title ordering, original/US aliases, season bounds, and anime title aliases.
- Import AniDB/TVDB episode crosswalks, explicit episode maps and split-season mappings. Ambiguous mappings are rejected instead of guessed.
- Download and refresh public metadata **inside the running container**. GitHub Actions does not host or refresh the production database.
- Store the index in a persistent Docker volume; rebuild a new SQLite file and atomically replace the old one only after a successful refresh.

## Quick start: Docker Compose

Prerequisites: Docker Engine, Docker Compose plugin, enough storage for downloading IMDb TSV archives and building a second SQLite index, and network access to upstream sources.

```bash
git clone https://github.com/Dizzybog/torrent-parse-match.git
cd torrent-parse-match
docker compose up -d --build
docker compose logs -f torrent-imdb-ai
```

The included [compose.yaml](compose.yaml) runs `torrent-imdb-ai` as container `torrent-parse-match`, with `restart: unless-stopped`, capped JSON logs, and a named volume `metadata` mounted at `/data`. The SQLite file is `/data/metadata.sqlite3`. The process runs as UID 10001 and listens on port 8765.

**Security:** The example binds `127.0.0.1:8765:8765`, so it is reachable from the Docker host but not directly from other machines. The application does **not** provide authentication; use an authenticated reverse proxy and HTTPS before exposing it externally. Change the port binding only if you understand the exposure.

The first metadata import can take considerable time and disk space. The API may start before the database is ready. Use the health endpoint to check readiness:

```bash
curl -i http://127.0.0.1:8765/health
```

During initial import the health/match endpoints may return HTTP 503. If a later refresh fails, the previous successful database remains available.

## HTTP API

### Match a torrent name

```bash
curl -sS -X POST http://127.0.0.1:8765/match \
  -H 'Content-Type: application/json' \
  -d '{"t_title":"Sword.Art.Online.S04E15.1080p.WEB-DL"}'
```

Or use GET:

```bash
curl -G 'http://127.0.0.1:8765/match' \
  --data-urlencode 'title=Dune.2021.2160p.WEB-DL'
```

Optional `year` and `series` parameters can be supplied when supported by the endpoint. A successful match returns one JSON object; an unmatched or ambiguous title returns `null`. Invalid input returns HTTP 400; an uninitialized metadata database returns HTTP 503.

A representative anime response **shape** (the example IDs/episodes depend on actual imported mappings):

```json
{
  "imdb_id": "tt2250192",
  "season": 4,
  "episode": 15,
  "sid": "tt2250192:4:15",
  "anime": {
    "anidb_id": "15146",
    "episode": 3,
    "asid": "15146:3"
  },
  "title": "Sword Art Online",
  "year": 2012,
  "category": "series",
  "matched_title": "Sword Art Online",
  "anime_ids": {},
  "parsed": {}
}
```

The actual `parsed` field contains RTN's parsed data, and `anime_ids` contains available cross-references. For non-anime matches, `anime` is `null`. If no episode is parsed, `season`/`episode` may be `null` and `sid` contains the IMDb ID alone. Anime episode translation requires a valid crosswalk; the API does not invent AniDB episodes.

### Health

```bash
curl -i http://127.0.0.1:8765/health
```

## Metadata sources and storage

| Source | Purpose |
|---|---|
| IMDb `title.basics.tsv.gz` | IMDb ID, canonical/original title, release year, media type |
| IMDb `title.akas.tsv.gz` | Regional/original/alternate titles |
| IMDb `title.episode.tsv.gz` | Maximum known series season |
| [animeApi v3](https://github.com/nattadasu/animeApi) | IMDb, AniDB, TVDB and Trakt identity relationships |
| [Anime-Lists](https://github.com/Anime-Lists/anime-lists) | AniDB ↔ TVDB seasons, episode offsets and explicit mappings |
| [AniDB anime titles](https://anidb.net/) | Japanese, romaji and English anime title aliases |

The local database includes `titles`, `seasons`, `anime_ids`, `anime_series_map`, `episode_map`, `anidb_titles`, and `metadata_state`. It does not require a running PostgreSQL database or an LLM.

### IMDb filtering and disk usage

Only IMDb records with these explicit `titleType` values are retained:

| Films / short-form | Series |
|---|---|
| `movie`, `tvMovie`, `short`, `tvShort`, `tvSpecial` | `tvSeries`, `tvMiniSeries` |

Entries with missing, unknown or unsupported types, including `tvEpisode` and `videoGame`, are **not** inserted into the `titles` index. Their AKA records are also excluded. An eligible record with an unknown release year is retained with year `0`.

IMDb AKA data is processed in bounded 20,000-row temporary batches rather than copying the complete AKA file into a temporary database table. Maximum-season metadata is retained only for included series. This reduces the persisted index and temporary import overhead, but **does not** eliminate the need to download/read the original IMDb archives. The exact size reduction depends on upstream data.

Every full refresh builds a fresh staging SQLite database; manually added data in the previous SQLite file is **not** carried over. A failed refresh leaves the previous database in place. Avoid running multiple refresh processes against the same target concurrently.

### Refresh schedule

The service checks the last successful `metadata_state.full_refresh` timestamp on startup and then every 24 hours. If no completed index exists or the timestamp is at least **7 days** old, it refreshes from public sources. On failure it retries after **1 hour**. Refreshing happens on your machine, not in GitHub Actions.

## Python / CLI

Install the project from source with Python 3.10+ and a Rust toolchain:

```bash
python -m pip install .
media-match --db metadata.sqlite3 refresh-local
media-match --db metadata.sqlite3 match 'Dune.2021.2160p.WEB-DL'
media-match --db metadata.sqlite3 serve --host 127.0.0.1 --port 8765
```

`serve` serves an existing database; `service` runs the HTTP API with the automatic refresh loop:

```bash
media-match --db metadata.sqlite3 service --host 127.0.0.1 --port 8765
```

Manual metadata import commands:

```bash
media-match --db metadata.sqlite3 import-imdb title.basics.tsv.gz title.akas.tsv.gz --episodes title.episode.tsv.gz
media-match --db metadata.sqlite3 update-imdb
media-match --db metadata.sqlite3 update-anime
media-match --db metadata.sqlite3 import-anime mappings.json
media-match --db metadata.sqlite3 import-crosswalk series.tsv episodes.tsv
```

These are alternatives to the automated Docker refresh, not required for a normal Compose installation.

Python integration:

```python
from MediaMatch import MetadataIndex
from MediaMatch.api import match_title

index = MetadataIndex("metadata.sqlite3")
result = match_title(index, "Dune.2021.2160p.WEB-DL")
print(result["imdb_id"] if result else None)
```

### Existing PTT / RTN interfaces

```python
from PTT import parse_title
from RTN import parse, RTN
from RTN.models import DefaultRanking, SettingsModel

print(parse_title("The.Simpsons.S01E01.1080p.BluRay.x265", False))
print(parse("Oppenheimer.2023.2160p.REMUX.HEVC").resolution)

rtn = RTN(SettingsModel(), DefaultRanking())
ranked = rtn.rank(
    raw_title="The Walking Dead S05E03 720p x264-ASAP",
    infohash="c08a9ee8ce3a5c2c08865e2b05406273cabc97e7",
)
print(ranked.rank, ranked.fetch)
```

Matching is an additional feature; it does not change the RTN ranking algorithm.

## Development and testing

```bash
python -m pip install . pytest
python -m pytest -q tests/test_media_match.py
python -m pytest -q -s tests/test_100_titles.py
docker build -t torrent-parse-match:test .
```

The 100-title integration fixture tests 20 film/series names × 5 release patterns (720p, 1080p, 2160p, WEB, BluRay, REMUX). It invokes the actual RTN parser and SQLite matcher, prints each `t_title`, expected/actual IMDb ID and SID, and checks for wrong IDs or exceptions. A recorded GitHub Actions run reported **100 correct, 0 null, 0 wrong/exceptions**. **Those IMDb IDs are synthetic fixture identifiers**; this is not a measured accuracy score on the complete IMDb corpus. Unit tests and an x86_64 Docker build have also passed in GitHub Actions.

The repository retains the original Rust/PTT/RTN test and quality tooling:

```bash
./scripts/quality_gate.sh
./scripts/quality_gate.sh --full
```

For more details, see [Docker and local metadata](docs/LOCAL_METADATA.md) and [parser benchmarks](benchmarks/README.md).

## Limitations / roadmap

- Full IMDb import, live upstream source compatibility, real-world matching precision/recall, and storage footprint need production-scale verification.
- ARM64 Docker builds and Oracle Free Tier deployment have not yet been verified.
- The SQLite matching logic follows selected PostgreSQL IMDb V4 and StremThru ideas, but is **not guaranteed to be behaviorally identical**. Complex anime split seasons, special episodes and duplicate-title collisions need more fixture coverage.
- The API is deterministic and local; it does **not** currently run Ollama or another AI model.
- Do not expose the unauthenticated API directly to the public Internet.

## Credits and license

- [PTT](https://github.com/dreulavelle/PTT) and [RTN](https://github.com/dreulavelle/rank-torrent-name) upstream implementations.
- [torrent-parse-rank](https://github.com/g0ldyy/torrent-parse-rank) Rust-based parsing/ranking foundation.
- [StremThru](https://github.com/MunifTanjim/stremthru) and [Anime-Lists](https://github.com/Anime-Lists/anime-lists) for anime mapping conventions and data.
- [IMDb non-commercial datasets](https://developer.imdb.com/non-commercial-datasets/) for title metadata.

The repository declares GPL-3.0 licensing. Observe IMDb and other upstream source licensing and usage terms.
