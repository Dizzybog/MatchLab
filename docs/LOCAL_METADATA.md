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

## Anime response fields

For an episode torrent, the single-match JSON contains IMDb coordinates and
an AniDB mapping when it can be resolved:

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
  }
}
```

The response also contains title/category/parser metadata. This example
illustrates a TVDB offset of 12; it is not a live API result.

The AniDB episode can differ from the IMDb/TVDB episode. When no safe AniDB
mapping exists, `anime` is `null`; the matcher does not invent an ASID.
For accurate multi-season anime mapping, import the AniDB/TVDB episode crosswalk
as well as IMDb↔AniDB identities. This PR does not yet implement the full
PostgreSQL split-season/Trakt mapping semantics.

## Import the PostgreSQL anime crosswalk (required for both numbering systems)

The public anime-offline-database is **not** sufficient for accurate absolute
episode conversion. Export your StremThru tables on the Oracle server:

```bash
mkdir -p data/crosswalk
sudo docker exec -i postgres_stremthru psql -U postgres -d postgres -c \
  "COPY (SELECT imdb,anidb,tvdb,trakt_season FROM public.anime_id_map WHERE imdb IS NOT NULL AND anidb IS NOT NULL) TO STDOUT WITH (FORMAT csv, HEADER true, DELIMITER E'\\t')" \
  > data/crosswalk/anime_id_map.tsv

sudo docker exec -i postgres_stremthru psql -U postgres -d postgres -c \
  "COPY (SELECT anidb_id,tvdb_id,anidb_season,tvdb_season,start,\"end\",\"offset\",map FROM public.anidb_tvdb_episode_map) TO STDOUT WITH (FORMAT csv, HEADER true, DELIMITER E'\\t')" \
  > data/crosswalk/episode_map.tsv

uv run media-match --db data/metadata.sqlite3 import-crosswalk \
  data/crosswalk/anime_id_map.tsv data/crosswalk/episode_map.tsv
```

The importer expects **tab-separated** exports with a header. For fields that
contain tabs, use PostgreSQL's CSV quoting. Run the command after the initial
metadata refresh; subsequent local refreshes preserve the crosswalk tables.

The resolver supports both `One Piece Episode 1067` (absolute AniDB numbering)
and `One Piece S23E02` (season-based numbering). It translates through the
actual imported episode crosswalk and returns the same `sid`/`asid` only
**when the data explicitly establishes that equivalence**. It does not assume
these two particular numbers are equivalent.

Split Trakt seasons are supported where two AniDB identities share a TVDB
series/season and have different positive `trakt_season` values. This is a
focused port of the SQL mapping rules, not yet a complete parity guarantee
for every fallback in the original PostgreSQL functions.
