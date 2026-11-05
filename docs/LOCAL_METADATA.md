# Native Rust metadata lifecycle

The production Docker image contains only the Rust HTTP API and native metadata importer. No Python runtime is required.

## Setup

Run `docker compose up -d`. The service uses a persistent volume at `/data` and listens on localhost port 8765. Without a populated SQLite database, `/health` returns 503.

Automatic initialization and refresh use seven built-in public HTTPS URLs. No environment variables are required. Set `MATCHLAB_METADATA_URLS` only to override all seven sources, in this order:

1. IMDb `https://datasets.imdbws.com/title.basics.tsv.gz`
2. IMDb `https://datasets.imdbws.com/title.akas.tsv.gz`
3. IMDb `https://datasets.imdbws.com/title.episode.tsv.gz`
4. IMDb ratings `https://datasets.imdbws.com/title.ratings.tsv.gz`
5. animeApi `https://raw.githubusercontent.com/nattadasu/animeApi/refs/heads/v3/database/animeapi.tsv`
6. Anime-Lists `https://raw.githubusercontent.com/Anime-Lists/anime-lists/refs/heads/master/anime-list.xml`
7. AniDB titles `https://anidb.net/api/anime-titles.xml.gz`

`MATCHLAB_METADATA_REFRESH_HOURS` defaults to 168 and supports 1–720 hours. The Rust service starts the first refresh on startup, then repeats at the interval. Refreshes are serialized; failures are logged and the last published SQLite database is preserved. The metadata importer builds a staging database, validates integrity, builds and verifies the matching Tantivy generation under `metadata.sqlite3.tantivy/`, and only then atomically replaces the published SQLite database. Each SQLite snapshot records its Tantivy generation ID. Old in-flight readers retain their old generation; no successful refresh rebuilds the newly published index. Historical completed generations are retained for at least seven days, preserving two previous generations.

Alternatively, import six local files using `matchlab-server refresh-metadata <database.sqlite3> <basics.tsv.gz> <akas.tsv.gz> <episodes.tsv.gz> <animeapi.tsv> <anime-list.xml> <anidb-titles.xml.gz>`.

The historical curated Anime-Lists overrides are preserved as `data/anime-list.override.xml` for reference. **They are not automatically applied by the current Rust refresh pipeline.** The default import consumes the supplied Anime-Lists XML only; integrate these overrides explicitly before relying on their special-case mappings.

## Verification

```bash
RUSTFLAGS='-D warnings' cargo test --workspace
docker build -t matchlab:local .
curl http://127.0.0.1:8765/health
```

The container has no built-in authentication. Keep the localhost port binding or use an authenticated HTTPS reverse proxy.

The HTTP listener starts while metadata initializes. Docker health checks return HTTP 503 until the first complete import succeeds. The initial download requires outbound HTTPS connectivity and sufficient disk space.
