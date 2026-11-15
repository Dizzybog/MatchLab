# MatchLab

**MatchLab** is a self-hosted API that identifies movies, TV shows and anime from torrent release names. It parses the title and episode information, finds the matching IMDb title and, where verified, maps anime episodes to AniDB.

For example, `Breaking.Bad.S02E03.1080p.WEB-DL` is parsed as a series title, season 2, episode 3 and quality tags, then matched against a local metadata index. **MatchLab does not find or download torrents.**

- Written in Rust, with **Tantivy** for title search and **SQLite** for metadata
- HTTP API and web dashboard
- Strict matching by default: no guessing when metadata is missing or ambiguous
- Optional local machine learning that improves the parser over time

## Quick start

Requires Docker with Docker Compose. Use the bundled [compose.yaml](compose.yaml), or this minimal version:

```yaml
services:
  matchlab:
    image: ghcr.io/dizzybog/matchlab:latest
    restart: unless-stopped
    environment:
      MATCHLAB_DB: /data/metadata.sqlite3
      MATCHLAB_HOST: 0.0.0.0:8765
    ports:
      - "127.0.0.1:8765:8765"
    volumes:
      - metadata:/data

volumes:
  metadata:
```

```bash
docker compose up -d
curl http://127.0.0.1:8765/health
```

The first run downloads public IMDb and anime metadata and builds the index, which takes a while. `/health` returns **503** until it is ready and **200** after. The dashboard is at <http://127.0.0.1:8765/dash>.

To update: `docker compose pull && docker compose up -d`.

## Example

```bash
curl -X POST http://127.0.0.1:8765/match \
  -H 'Content-Type: application/json' \
  -d '{"title":"Supernatural.2005.S01E01-E22.1080p"}'
```

```json
{
  "sid_start": "tt0460681:1:1",
  "sid_end": "tt0460681:1:22",
  "asid_start": null,
  "asid_end": null
}
```

A **SID** is an IMDb ID, optionally with season and episode. An **ASID** is a verified AniDB episode. Unmatched values are `null`.

## Documentation

| Topic | Document |
| --- | --- |
| Endpoints, requests and responses | [API reference](docs/API.md) |
| How matches are decided (strict mode, years, collections, anime parts) | [Matching rules](docs/MATCHING_RULES.md) |
| Settings, dashboard, logging and security | [Configuration](docs/CONFIGURATION.md) |
| Metadata download and refresh | [Metadata lifecycle](docs/LOCAL_METADATA.md) |
| Optional machine learning | [ML overview](docs/ML_OVERVIEW.md) · [ML scaling](docs/ML_SCALING.md) · [ML regex engine](docs/ML_REGEX_ENGINE.md) |
| Sharing learned rules on GitHub | [Community sharing setup](docs/COMMUNITY_SHARING_SETUP.md) · [Community learning](docs/COMMUNITY_LEARNING.md) |
| Parser background | [Release parser audit](docs/RELEASE_PARSER_AUDIT.md) · [Anime parser research](docs/ANIME_PARSER_RESEARCH.md) |
| Performance | [Performance](docs/PERFORMANCE.md) · [Tantivy runtime](docs/TANTIVY_RUNTIME.md) |

## Security

**The API and dashboard have no built-in login.** Keep the port bound to localhost, or put an authenticated HTTPS reverse proxy in front of it. Diagnostic and learning history can contain release names, so protect the Docker volume. See [Configuration](docs/CONFIGURATION.md#security).

## Build and test

```bash
git clone https://github.com/Dizzybog/MatchLab.git
cd MatchLab
RUSTFLAGS="-D warnings" cargo test --workspace --locked
docker build -t matchlab:local .
```

## Credits and license

MatchLab is an independent project informed by [StremThru](https://github.com/MunifTanjim/stremthru), [Comet](https://github.com/g0ldyy/comet), [AIOStreams release parser](https://github.com/AIOStreams/release-parser), [parse-torrent-title](https://github.com/TheBeastLT/parse-torrent-title), [Rakun](https://github.com/lowlighter/rakun) and [FTRL-Proximal research](https://research.google/pubs/ad-click-prediction-a-view-from-the-trenches/). It is not affiliated with these projects, whose authors retain their own credits and licenses. See [Credits](docs/CREDITS.md).

Licensed under **GPL-3.0-or-later**. Metadata sources have their own terms.
