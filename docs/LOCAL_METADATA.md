# Docker: autonomous metadata API

The container downloads and maintains all IMDb and anime metadata **on the machine
running Docker**. No PostgreSQL, cron, manually exported files or API keys are required.

## Run

```bash
docker compose up -d --build
docker compose logs -f torrent-imdb-ai
```

A persistent Docker named volume `metadata` stores `/data/metadata.sqlite3`.
The image runs as an unprivileged user (UID 10001) and listens on port 8765.
The example Compose file binds to `127.0.0.1:8765`; configure your own
authenticated reverse proxy if public access is needed.

## Automatic refresh lifecycle

- At **every startup**, inspect `metadata_state.full_refresh` in SQLite.
- If the database is missing, incomplete, or the last successful full update
  is **7 days old or older**, download and rebuild all metadata immediately.
- While running, recheck every **24 hours**. Refresh only if at least 7 days old.
- After a failed download/import, keep the last good database and retry in 1 hour.
- A full successful refresh timestamp is written **only after** all required
  sources were imported. A temporary SQLite database is atomically swapped
  into place. Data survives restarts through the Docker volume.
- The API starts while the first import runs. `GET /health` responds
  with HTTP 503 until the initial import succeeds; `POST /match` also
  responds with HTTP 503 rather than incorrectly returning `null`.
  Stale but previously successful metadata remains available.

## Public metadata sources

- IMDb official non-commercial bulk TSV files: `title.basics`,
  `title.akas`, and `title.episode`.
- StremThru's animeApi v3 source:
  `https://raw.githubusercontent.com/nattadasu/animeApi/refs/heads/v3/database/animeapi.tsv`
  for IMDb/AniDB/TVDB/Trakt identities.
- StremThru's AniDB title source:
  `https://anidb.net/api/anime-titles.xml.gz`
  for Japanese/romaji/English aliases.
- StremThru's Anime-Lists source:
  `https://raw.githubusercontent.com/Anime-Lists/anime-lists/refs/heads/master/anime-list.xml`
  for AniDB/TVDB season, absolute episode and offset mappings.

SQLite tables are `titles`, `seasons`, `anime_ids`,
`anime_series_map`, `episode_map`, `anidb_titles`, `metadata_state`.

## API

```bash
curl http://127.0.0.1:8765/health
curl -X POST http://127.0.0.1:8765/match \
  -H 'Content-Type: application/json' \
  -d '{"t_title":"One Piece S23E02"}'
```

Returns one IMDb match with season/episode and AniDB mapping, or JSON
`null` if the mapping is unavailable or ambiguous. Both `Episode 1067`
and `S23E02` are supported through the imported crosswalk; the two are
**not presumed equivalent** without matching metadata.

## Known limitations

StremThru's embedded XML override rules, direct episode offsets, inferred
TVDB season boundaries, and explicit episode maps have been ported. Some
special episode-before semantics and additional StremThru title-ranking
heuristics still need parity testing. The Docker build and complete IMDb
bulk refresh have not been validated on Oracle ARM in this PR.
