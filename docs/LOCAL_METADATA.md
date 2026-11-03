# Local IMDb + anime metadata refresh (Oracle / Linux)

Metadata updates run **on your server**, not in GitHub Actions. The API returns
one IMDb match object or JSON `null` (never a candidate list).

## Install

From the repository root:

```bash
uv sync --group dev
uv run maturin develop --release
```

## Initial refresh

```bash
mkdir -p data
uv run media-match --db "$PWD/data/metadata.sqlite3" refresh-local
```

The refresh downloads IMDb basics/AKAs/episodes and anime-offline-database,
builds a temporary SQLite database **in the same directory**, then atomically
replaces the live database. The existing database remains intact on download
or import failure. Keep at least enough free disk space for two database copies.

This download is substantial. The first run can take a long time. AniDB/TVDB
episode crosswalks and supplemental mappings must still be imported separately;
the automatic anime source is not a complete equivalent of StremThru's
`anime_id_map` and `anidb_tvdb_episode_map`.

## Weekly local schedule

Install a cron entry for the user that owns the checkout:

```bash
crontab -e
```

Add (replace `/opt/torrent-parse-match` with the real checkout path):

```cron
23 5 * * 1 cd /opt/torrent-parse-match && /usr/bin/flock -n /tmp/media-match-refresh.lock /usr/bin/env uv run media-match --db /opt/torrent-parse-match/data/metadata.sqlite3 refresh-local >> /opt/torrent-parse-match/data/refresh.log 2>&1
```

`uv` must be in cron's PATH; if installed in `~/.local/bin`, use its absolute
path instead of `/usr/bin/env uv`. Run the command manually first.

## Serve locally

```bash
uv run media-match --db "$PWD/data/metadata.sqlite3" serve --host 127.0.0.1 --port 8765
```

```bash
curl -G 'http://127.0.0.1:8765/match' --data-urlencode 'title=Dune.2021.1080p'
curl -X POST 'http://127.0.0.1:8765/match' -H 'Content-Type: application/json' \
  -d '{"t_title":"Dune.2021.1080p"}'
```

The response is one match object, or `null` if no unique match can be
established. The HTTP server has no authentication. Do **not** expose it to
the public internet without an authenticated reverse proxy and rate limits.

## Limitations

This is a conservative matcher, **not** a complete port of the PostgreSQL V4
trigger. Complex country variants, the full reverse-suffix logic, and AniDB
episode-to-ASID selection still require parity tests against the original
SQL functions. The current API returns `anime_ids` when a crosswalk exists,
but does not promise an `asid` field. No match is preferable to a false ID.
