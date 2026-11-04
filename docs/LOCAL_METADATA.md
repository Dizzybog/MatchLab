# Native Rust metadata lifecycle

The production Docker image contains only the Rust HTTP API and native metadata importer. No Python runtime is required.

## Setup

Run `docker compose up -d`. The service uses a persistent volume at `/data` and listens on localhost port 8765. Without a populated SQLite database, `/health` returns 503.

To enable automatic initialization and refresh, configure `MATCHLAB_METADATA_URLS` with exactly six comma-separated HTTPS URLs, in this order:

1. IMDb `title.basics.tsv.gz`
2. IMDb `title.akas.tsv.gz`
3. IMDb `title.episode.tsv.gz`
4. animeApi TSV
5. Anime-Lists XML
6. AniDB titles XML gzip

`MATCHLAB_METADATA_REFRESH_HOURS` defaults to 24 and supports 1–720 hours. The Rust service starts the first refresh on startup when URLs are configured, then repeats at the interval. Refreshes are serialized; failures are logged and the last published SQLite database is preserved. The metadata importer builds a staging database, validates integrity and atomically replaces the published database after successful import.

Alternatively, import six local files using `matchlab-server refresh-metadata <database.sqlite3> <basics.tsv.gz> <akas.tsv.gz> <episodes.tsv.gz> <animeapi.tsv> <anime-list.xml> <anidb-titles.xml.gz>`.

The historical curated Anime-Lists overrides are preserved as `data/anime-list.override.xml` for reference. **They are not automatically applied by the current Rust refresh pipeline.** The default import consumes the supplied Anime-Lists XML only; integrate these overrides explicitly before relying on their special-case mappings.

## Verification

```bash
RUSTFLAGS='-D warnings' cargo test -p matchlab-server
docker build -t torrent-parse-match:local .
curl http://127.0.0.1:8765/health
```

The container has no built-in authentication. Keep the localhost port binding or use an authenticated HTTPS reverse proxy.
