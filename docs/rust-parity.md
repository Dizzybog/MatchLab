# Python–Rust matching parity

The Python production service and experimental Rust service must read the **same**
initialized SQLite metadata snapshot. Do not compare against separate database
refreshes: a metadata difference would be indistinguishable from a matching bug.

## Start the services

Python: port 8765, using the existing deployment.

Rust:
```bash
docker build -f Dockerfile.rust -t matchlab-rust:local .
docker run --rm -p 127.0.0.1:8766:8766 \
  -v /absolute/path/to/metadata.sqlite3:/data/metadata.sqlite3:ro \
  matchlab-rust:local
```

The SQLite database must have completed the initial metadata refresh. Both
services can read the same file concurrently. Do not delete/replace the file
while comparing results.

## Compare

```bash
python3 scripts/compare_python_rust.py tests/parity_titles.txt \
  --python http://127.0.0.1:8765 --rust http://127.0.0.1:8766 \
  --output parity-report.json --strict
```

The script compares **imdb_id, season, episode, sid, anidb_id, AniDB
episode and asid**; presentation fields are deliberately ignored. It writes
every input and both outputs to a JSON report. Without `--strict`, identity
mismatches are reported but do not fail the command; HTTP errors always fail.

Known intentional divergence: Rust selects the highest-ranked IMDb candidate
on a tie, whereas the current Python service may return null. These cases
should be reviewed, not blindly treated as Rust regressions.

The initial corpus is a small smoke set, **not** evidence of parity across
100 real films, 100 real series and 100 real anime releases. Collect an
appropriately licensed or user-owned real-world corpus and review mismatches
before promoting the Rust runtime.
