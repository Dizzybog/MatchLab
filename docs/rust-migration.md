# Rust backend migration

This branch introduces an **experimental** native Rust HTTP server. It is intentionally
not enabled in the production Docker image. The current Python service remains the
reference implementation until full matching parity and metadata-refresh parity are
proven.

## Current implementation

- Uses the existing `ptt-core` Rust parser directly.
- Opens the existing SQLite metadata database in read-only mode.
- Exposes `GET /health`, `POST /match`, and `POST /debug/match`.
- Provides a first native implementation of IMDb title lookup and verified
  AniDB-to-TVDB episode mapping.
- Rejects ambiguous anime crosswalks rather than guessing.
- Runs on a separate port (8766 by default), allowing side-by-side testing.

## Run against an existing metadata database

```bash
MATCHLAB_DB=/path/to/metadata.sqlite3 MATCHLAB_HOST=127.0.0.1:8766 \
  cargo run --release -p matchlab-server
curl -s http://127.0.0.1:8766/match \
  -H 'Content-Type: application/json' \
  -d '{"title":"Dune.2021.2160p.WEB-DL"}'
```

## Parity gaps before replacing Python

- Full V4 exact/prefix/reverse-suffix/half-prefix ranking, country aliases,
  title-original/US ranking, season caps and tie resolution.
- Title-specific anime parts, absolute numbering, split Trakt seasons and
  explicit Anime-Lists episode mapping expressions.
- Metadata import, download, refresh, atomic database swap and retries.
- MatchLab static dashboard and diagnostic response parity.
- Production logging, graceful shutdown, input validation and Docker packaging.
- Differential tests against the Python matcher using real SQLite fixtures.

**Do not merge into main as a production replacement until these are resolved.**
Rust-only tests and successful compilation are necessary but insufficient:
match parity must be demonstrated with a broad test corpus.

## Regression coverage: 300 release titles

`crates/matchlab-server/tests/release_300.rs` generates and parses **100 movie**, **100 TV**, and **100 anime** release-name variations. This is a parser smoke suite; it does **not** establish matching parity. For parity acceptance, run both implementations against the same populated SQLite fixture and assert IMDb ID, SID, ASID, season, episode, ambiguity rejection and candidate ordering. Include country aliases, anime-specific parts, absolute numbering, mapped and unmapped episodes, and live-action title collisions.

The native matching engine currently uses simplified IMDb and anime lookups and cannot replace the Python implementation. In particular, the Rust server currently has no metadata importer or dashboard HTML route. Do not treat passing parser tests as approval to merge.
