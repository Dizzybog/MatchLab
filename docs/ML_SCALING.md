# One setting for MatchLab's local machine learning

Configure local ML with **one** environment variable:

```yaml
environment:
  MATCHLAB_ML_SIZE_GB: "10"
```

- `0` (default): ML disabled; ordinary Rust parsing, IMDb/AniDB matching
  and Tantivy lookup continue without ML.
- `1` to `128`: enable native-Rust background learning and reserve that
  many **GiB** of best-effort ML data capacity.
- There is no need to set example, cache, queue, batch-size or individual
  storage variables. Only `MATCHLAB_ML_SIZE_GB` is needed to enable ML.

## How the total budget is divided

| Component | Share | Used for |
| --- | ---: | --- |
| Training evidence | 50% | Verified observations, FTRL state, candidate rules, archives |
| Versioned result cache | 30% | Separate SQLite cache; stale results reparse/rematch |
| Training queue and history | 20% | Durable unprocessed observations and 1,000-row UI history |

Each disk allocation is represented internally in **bytes**, not rounded
up to a full GiB, so even a 1 GiB total budget splits correctly.
The SQLite cache and evidence workers use occupied-page accounting to
apply their budgets. The queue writer maintains a constant-time count and
an estimate of queued payload bytes through SQLite triggers, rather than
scanning a potentially million-row queue on every insert. Older observations
are dropped first when over budget, **never a public parsing/matching response**.

**Important:** These are best-effort **application budgets**, not strict
filesystem quotas. SQLite WAL files, indices, checkpointing, the 1,000-row
dashboard history, and independent IMDb/AniDB metadata may require additional
disk space. The cache is an accelerator; it can stop accepting writes if full.
Verified active rules and their archives are not sacrificed to hit a byte
target. Monitor actual filesystem free space separately.

## Automatic scaling

Let `G` be the configured GB value:

- Training observations: `min(G * 50,000, 2,000,000)`
- Persistent cache entries: **1.0 ×** training observations
- Durable pending queue: **1.8 ×** training observations
- In-memory hot cache: up to **2,048 entries / 32 MiB**, whichever is reached
  first; smaller budgets use fewer entries
- Asynchronous cache-write queue: `clamp(examples / 100, 256, 4,096)`
- Training batch size: selected from 16/32/64/96 by capacity, never higher
  than 128. Workers remain CPU-pacing constrained.
- Source/field evidence pools: dynamically sized fractions of the training
  corpus, with hard caps for expensive or safety-sensitive operations.

| Total ML size | Training examples | Cache entries | Pending queue | Evidence budget | Cache budget | Queue/history budget |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 GiB | 50,000 | 50,000 | 90,000 | 0.5 GiB | 0.3 GiB | 0.2 GiB |
| 2 GiB | 100,000 | 100,000 | 180,000 | 1 GiB | 0.6 GiB | 0.4 GiB |
| 10 GiB | 500,000 | 500,000 | 900,000 | 5 GiB | 3 GiB | 2 GiB |
| 40 GiB | 2,000,000 | 2,000,000 | 3,600,000 | 20 GiB | 12 GiB | 8 GiB |

These count limits are **upper bounds**, not commitments: the allocated
byte budgets may limit retention earlier when titles, evidence or cached
responses are large. The relationship between examples and bytes is only
an initial planning estimate. The system measures occupied SQLite pages for
cache/evidence and retains room for filesystem overhead.

## Safety controls do not scale with storage

A larger disk budget must **not** increase public matching concurrency,
regex activation without independent proof, IMDb/AniDB ambiguity tolerance,
GitHub sharing frequency, or PCRE/regex complexity. The same field-specific
validation, negative controls, shadow-scoring and failure isolation apply
at every size. The FTRL model remains a fixed-size online model; GB
controls the **retained observations**, not GPU use or arbitrary RAM growth.

Optional independent switches are deliberately not required for normal
operation: `MATCHLAB_ML_LIVE_REGEX_ENABLED=false` disables live regex
publication while learning continues; `MATCHLAB_ML_CACHE_ENABLED=false`
disables result caching; GitHub sharing needs explicit opt-in and a token.
These are safety/feature permissions, **not** ML capacity knobs.

## Upgrade from older settings

The single setting takes precedence whenever it is present. For example,
an older Compose file may still export `MATCHLAB_ML_ENABLED=true` plus
individual limits: adding `MATCHLAB_ML_SIZE_GB=10` makes the new GB policy
authoritative for the learner, storage, cache and queue.

When `MATCHLAB_ML_SIZE_GB` is absent, older `MATCHLAB_ML_ENABLED`,
`MATCHLAB_ML_TRAINING_MAX_EXAMPLES`, `MATCHLAB_ML_QUEUE_MAX_ITEMS`,
`MATCHLAB_ML_CACHE_MAX_ENTRIES`, `MATCHLAB_ML_MAX_STORAGE_GB`,
`MATCHLAB_ML_CACHE_MAX_STORAGE_GB` and
`MATCHLAB_ML_TRAINING_BATCH_SIZE` remain recognized for compatibility.
Migrate to the single setting to get coherent shared budgeting.

**No database reset is necessary.** Existing learning evidence, verified
regex rules and cached responses remain on the same persistent Docker
volume. When reducing the size, cleanup evicts optional old observations
and stale cache rows incrementally; it does not discard active validated
rules. Ensure the Docker volume has headroom during migration.

## Inspect effective limits

Call `GET /api/ml/status` and inspect `capacity`. It reports:
`size_gb`, `max_examples`, `queue_max_items`,
`cache_max_entries`, `learning_storage_limit_mib`,
`cache_storage_limit_mib`, `queue_storage_limit_mib` and
the derived hot-cache/worker capacities. Other status sections expose
model diagnostics, queue backlog, cache hits/misses, dropped observations
and verified rule counts. The capacity report also identifies when
legacy configuration is in use.
