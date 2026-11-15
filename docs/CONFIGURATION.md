# Configuration, dashboard and security

## Settings

| Setting | Default | Description |
| --- | --- | --- |
| `MATCHLAB_DB` | `/data/metadata.sqlite3` | Local metadata database |
| `MATCHLAB_HOST` | `0.0.0.0:8765` in Docker | API listener |
| `MATCHLAB_METADATA_REFRESH_HOURS` | `168` | Metadata refresh interval |
| `MATCHLAB_MAX_CONCURRENT_MATCHES` | `4` | Maximum concurrent matches (example uses 2) |
| `MATCHLAB_ML_SIZE_GB` | `0` | Single local ML control: 0=off, 1–128 GiB total budget; automatically scales examples, cache and queue |
| `MATCHLAB_ML_LIVE_REGEX_ENABLED` | `true` | Optional emergency opt-out: train without activating new regex rules |
| `MATCHLAB_ML_CACHE_ENABLED` | `true` when ML is on | Optional opt-out for cached results |
| `MATCHLAB_ML_CACHE_STATE_FILE` | `/data/ml-result-cache.sqlite3` | Advanced cache-path override |
| `MATCHLAB_COMMUNITY_LEARNING_ENABLED` | `false` | Enable optional GitHub reporting |
| `MATCHLAB_COMMUNITY_GITHUB_TOKEN` | Unset | Fine-grained PAT with Issues Read/write on upstream (owner), or classic `public_repo` PAT for external contributors; validated on startup |

More settings are documented in [Metadata lifecycle](LOCAL_METADATA.md), [API reference](API.md) and [Community Learning](COMMUNITY_LEARNING.md).

## Docker resources and startup

The README uses a minimal Compose file. The bundled [compose.yaml](../compose.yaml) adds resource limits like these:

```yaml
    cpus: 2.0
    mem_limit: 6g
    pids_limit: 256
    environment:
      MATCHLAB_MAX_CONCURRENT_MATCHES: "2"
      MATCHLAB_STRICT_MODE: "true"
```

The first run downloads public IMDb and anime metadata and builds the local index. This needs an internet connection, disk space and time. `/health` returns **503** until the index is ready and **200** when matching is available. Later metadata refreshes normally run every seven days. The dashboard is at [`http://127.0.0.1:8765/dash`](http://127.0.0.1:8765/dash). The root path redirects to `/dash` for old bookmarks. The root path `/` redirects to `/dash` for old bookmarks.

The example is limited to two CPUs and 6 GB RAM; these are starting limits, not guaranteed minimum requirements.

Follow startup with `docker compose logs -f matchlab`.

## Dashboard

The dashboard displays verified IMDb IDs as links to their IMDb title pages, and AniDB IDs (including multiple anime segments) as individual links to AniDB. Links open in a new tab; API identifiers remain unchanged.

The dashboard retains **the latest 500 detailed entries per status** (matched, unmatched and errors); **Requests** displays the 500 most recent entries overall. This keeps rare errors visible even during high match throughput. Each history filter supports paging through its 500-entry window; independent 14-day counters remain complete. ML training observations use a separate queue and are not deleted by dashboard retention.

## Logging

`ERROR` remains at error level. MatchLab's formerly `WARN`-level nonfatal diagnostics (metadata row skips, optional learning/cache/history failures and similar events) are emitted at `DEBUG`. Set `RUST_LOG=matchlab_server=debug` to inspect them; the default `info` level does not print them.

## Security

**The API and dashboard have no built-in login.** The example only binds the port to localhost. Use an authenticated HTTPS reverse proxy before exposing it publicly. Learning and diagnostic history can contain private release names; protect the Docker volume and never commit the GitHub token.
