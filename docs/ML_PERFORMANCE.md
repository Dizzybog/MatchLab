# ML runtime performance: Rust-only optimization

MatchLab's FTRL-Proximal model is intentionally small (16,384 feature slots,
128 KiB of model parameters). In ordinary deployments the expensive work is
instead reading training evidence, evaluating regex candidates, compiling
PCRE2 engines, and replaying complete parser results to prove non-regression.

## What is optimized

- **Immutable replay generations:** `replay_handler_rules` shares a bounded
  six-entry LRU of complete, JIT-compiled parser generations. Cache keys include
  every original-handler patch, ordered addition and correction **exactly**.
  Compilation happens only after a cache miss and outside the cache mutex.
  Both native public parsing and rule-admission validation remain unchanged.
- **Original rule catalogue:** Generated handler JSON is parsed once per
  process; the general learner indexes sparse source-handler IDs once rather
  than linearly scanning hundreds of handlers for every hypothesis.
- **General and corrective evidence revision:** SQLite triggers maintain
  transactional change counters, replacing repeated full-table SHA scans and
  repeated general-rule `GROUP BY` queries. Each memoized review is scoped to
  the exact database path, its evidence revision, and all relevant native and
  learned parser generations. In-memory databases bypass cross-connection
  caching. A rollback also rolls back the counter. Timestamp-only corrective
  updates do not invalidate a semantically unchanged review.
- **Rejected rule review:** Negative general-regex replay results are retained
  for the same full evidence/parser generation, with a bounded 4,096-key cap.
  Any new evidence or changed rule generation forces revalidation. Candidate
  rotation remains in place; no promotion threshold, negative control, or
  protected corpus is skipped.
- **FTRL feature extraction:** A per-release context shares lowercase text,
  technical markers and deterministic holdout membership across field
  observations and all unknown-token score candidates. FNV-1a hashes are
  computed directly from byte slices instead of temporary formatted Strings.
  Feature IDs are still sorted and unique as before, and their precise values
  are protected by differential regression tests.
- **SQLite hot path:** High-frequency evidence/dedup queries reuse SQLite
  prepared statements. Training still commits model state and queue cursor in
  one transaction. On busy/failure, the worker retries without acknowledging
  uncommitted jobs.
- **Training backlog:** The history writer already maintains
  `ml_queue_metrics.total` with triggers. The learner now subtracts the
  usually-small already-acknowledged tail rather than counting an enormous
  pending queue on every burst. Older history databases without metrics
  continue using the original count query.
- **No wasted canonical candidate parse:** General field induction returns
  early if the original parser supplied no suitable handler provenance; it
  invokes the public wrapper only after selecting a plausible mutation.

## Tantivy vs SQLite: use the existing search engine for what it indexes

MatchLab's existing Tantivy index is the **immutable metadata generation**.
It already serves IMDb/AniDB title, prefix, suffix, alias and external-ID
lookups, with generation-scoped caches. Preserve this architecture and use it
fully for metadata title retrieval.

The ML worker's observations, pending jobs, dedup keys, evidence revisions,
episode crosswalk proofs and accepted regex records are **mutable,
transactional, exact-key** workloads, not text relevance search. The
existing Tantivy index does not contain ML evidence or episode crosswalk rows.
Migrating this work to Tantivy would require a new mutable inverted index,
additional writes/segment merges and separate consistency checks. It does
not remove SQLite's transactions or the requirement to reparse evidence
against the current parser generation. Adding such an index solely for ML
would likely *increase* CPU consumption.

A future Tantivy-backed ML search layer should only be introduced if real
profiling identifies a high-frequency, **text-retrieval** evidence query that
cannot be served efficiently with the existing indexed SQLite schema.

## Safety invariants

1. Same original parse and complete JSON output for identical titles.
2. Same FTRL hash indices, update ordering, labels and holdout assignments.
3. Same native and learned parser transforms and output field identities.
4. No accepted regex without all prior independent positive, negative,
   cross-field and protected-release replay checks.
5. After evidence insert/update/delete, rule change or parser rebuild,
   cached validation is invalidated; no stale rule can be promoted.
6. No new HTTP matching dependency, no Python ML runtime, no external API key.
7. SQLite transaction rollback restores both the evidence and its revision.

## Verification

```sh
RUSTFLAGS="-D warnings" cargo test --workspace
RUSTFLAGS="-D warnings" cargo check --workspace --all-targets
cargo bench -p matchlab-parser --bench matchlab_parser_bench
cargo run --release -p matchlab-server -- verify-native-ml
cargo run --release -p matchlab-server -- benchmark-native-ml
```

The `benchmark-native-ml` command reports synthetic FTRL training,
prediction, canonical parsing, first full-regex replay and average cached
replay timing. Both the x86-64 and native ARM64 CI jobs execute the benchmark.
The result is a *microbenchmark*, not a CPU savings percentage or a claim
about production match quality. For production assessment, compare matched
requests/sec, CPU-seconds per 1,000 queued observations, p50/p95 HTTP latency
under background training, cache hit ratio, promotion decisions and the
proportion of stale/retired candidates using the same representative titles.

`GET /api/ml/status` exposes `compiled_parser_replay` with the bounded
cache size, hit count and actual compile count. No raw private releases,
model secrets or training evidence are exposed by this diagnostic.

Rust source is the sole production runtime for these optimizations. The
existing community-contribution scripts and GitHub automation are independent
of normal local parsing/matching.
