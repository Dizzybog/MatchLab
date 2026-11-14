# Startup and live API accuracy audit

Measured on 2026-11-14 against **https://MatchLab.notprod.fyi** and an isolated local release build. The deployed service was not restarted or reconfigured.

The subsequent [authenticated production-debug review](PRODUCTION_DEBUG_REVIEW.md)
uses retained real requests, adds parser/matcher corrections, and distinguishes
verified defects from heuristic alerts and unknown original request overrides.

## Findings

- Existing-index startup is fast in the controlled local fixture: approximately **14 ms** to readiness. Lazy parser initialization dominates the first parse, approximately **45–55 ms**, versus **0.5–0.6 ms** for repeated identical parses.
- The deployed matcher passes **300/300 ordinary release variants**, but only **66/78 more difficult source expectations**. Named anime parts and conflicting same-name identities are the important gaps.
- The deployed parser passes **1,591/1,635 explicit source assertions**. The newer local parser passes all extracted assertions. These are regression-fixture agreement measurements, not population accuracy estimates.
- Actual AniDB alias spelling exposed a gap in the tests: Stone Ocean uses **JoJo`s** with a backtick, whereas the fixture used a straight apostrophe. Query-time word normalization rejected the real alias in strict mode despite an identical compact index key.
- An open-ended anime crosswalk could select an old same-name OVA instead of a series with independently verified IMDb episode bounds. The follow-up preserves stronger episode evidence while retaining verified named-part remapping and production-year precedence.

## Versions, scope and controls

| Target | Revision and conditions |
| --- | --- |
| Deployed API | `1d3d6687ced39151d036c58f2d40b68a2d1e44eb`, observed through `/api/community/status` before and after the campaign; local learning enabled |
| Initial local measurements | `9ab237525f53c580c48f9af9fddb9de8603075df`, the source from PR #144; subsequently merged as `e14754732f9398b2172b49044f87293815315caf` with the same tree |
| Local startup binary | Rust 1.100.0, optimized release, Linux x86_64; Intel Xeon Platinum 8573C; cgroup CPU quota 2 cores, 8 GiB memory limit; ML disabled, strict mode enabled |
| Graphify | Refreshed graph at `e147547`: 2,111 nodes, 6,210 edges, 80 communities; caller/impact analysis checked against current local source |

The main HTTP campaign performed **1,205 distinct `/parse` cases and 383 `/match` cases**. All 1,588 returned HTTP 200; no campaign case timed out or was rate limited. Additional small health, alias and debug probes investigated causes. Requests ran in two sequential streams with a 150 ms per-stream delay and a response-size cap; this was not a load test. Normal curl requests worked, while an initial Python urllib probe received Cloudflare error 1010 and was excluded from semantic scoring. TLS verification and inherited network configuration were retained.

The remote version is older than the code evaluated locally. Consequently, missing new hint fields and already-fixed parser regressions must not be mistaken for failures newly introduced by this follow-up. Conversely, an existing passing fixture does not prove the deployed metadata combination works.

## Startup measurements

The benchmark creates deterministic SQLite movie catalogs, retains the current refresh and override markers, and directs every possible download to a non-listening loopback port. It never downloads a production IMDb catalog. No-index construction runs once per size; completed-index restarts run five times per size. Health polling resolution is 10 ms.

| Synthetic IMDb aliases / works | Readiness, missing index | Restart readiness, median of 5 | First parse, median | Repeated parse, median | First match after parser probes | Repeated match, median |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 100 | 70.3 ms | 14.8 ms | 55.4 ms | 0.60 ms | 3.58 ms | 1.00 ms |
| 10,000 | 120.0 ms | 14.2 ms | 44.6 ms | 0.58 ms | 3.97 ms | 1.05 ms |
| 100,000 | 882.4 ms | 13.8 ms | 51.4 ms | 0.47 ms | 3.82 ms | 1.03 ms |

Logs show completed indexes typically reopening in **1–2 ms** (observed range 1–6 ms), without a SQLite title scan or rebuild. The maximum first parse was 114.6 ms among completed-index restarts and 130.1 ms including missing-index runs. Missing-index peak RSS was approximately 26, 37 and 67 MiB respectively. With metadata absent, liveness was available in 8.0 ms, parsing still worked, and matching returned the expected HTTP 503.

The table's first match uses an already initialized parser. A separate match-first run with the final follow-up binary and 100,000 aliases measured **47.6 ms median** for the first `/match` before any `/parse` (maximum 59.4 ms), versus **1.00 ms** warm. Across five restarts, readiness took 15.3 ms median and the first match completed 63.0 ms after process spawn. All seven match-first scenarios passed, including unavailable-metadata 503 behavior. This confirms that parser initialization, rather than reopening a completed index, dominates the first successful request in this fixture.

These measurements do **not** represent cold operating-system caches, a populated history/ML volume, a full IMDb catalog, anime-heavy metadata, container image pull time or the user's production hardware. Real initial bootstrap includes seven serial feed downloads, decompression/import, SQLite validation and aggregation, Tantivy construction, and atomic publication. No credible full-bootstrap duration can be inferred from the synthetic 100,000-row result. Existing-index startup does not warrant an architectural rewrite on this evidence. Pre-initializing the parser after binding HTTP is a possible first-request optimization; it would move work into startup rather than remove it.

## Every title test: inventory and execution

The baseline workspace's **497 Rust tests** were executed in a disposable worktree with temporary tracing. Production source contains no tracing changes. The trace recorded:

| Inventory | Count |
| --- | ---: |
| Recorded calls, including internal engines and batches | 4,187 |
| Distinct inputs including internal components | 1,759 |
| Distinct public parser inputs | 1,222 |
| Inputs confined to internal engine tests | 537 |
| Stable, non-translating, HTTP-eligible public baselines | 1,205 |
| Invalid trace lines or unstable public baselines | 0 |

The HTTP corpus excludes 16 replay-only inputs requiring a custom test engine and one empty input rejected by the public API. Internal component inputs are inventoried rather than misrepresented as complete releases. All 400 initially curated parser inputs were observed during actual execution. The collector also inventories the three server release-title tests; the full server source inventory covers 378 test functions, including metadata, alias, learning and matching tests whose fixture IDs are not necessarily real works.

Explicit parser labels were conservatively extracted from source assertions. Unsupported branches, predicates and loops are retained for manual review, never evaluated as arbitrary code or replaced with parser output. The 400-case extraction has 129 manual predicates and six unexpanded loops. Runtime tracing covers generated inputs independently of those restrictions.

The separate release corpus includes all 300 movie/TV/anime variants plus 11 committed TV coordinate examples. Its **311 cases and 722 assertions** were checked against already captured `/parse` responses, requiring no additional requests.

## Parser results

| Cohort | Deployed API | Local newer parser |
| --- | ---: | ---: |
| Primary cases with explicit assertions | 358/378 | 378/378 |
| Primary field assertions | 869/913 (95.18%) | 913/913 |
| Exact canonical title assertions within primary cohort | 43/57 | 57/57 |
| Additional release corpus cases | 311/311 | 311/311 |
| Additional release corpus field assertions | 722/722 | 722/722 |
| Combined cases with explicit expectations | 669/689 (97.10%) | 689/689 |
| Combined field assertions | 1,591/1,635 (97.31%) | 1,635/1,635 |

There are another 22 primary inputs without machine-readable correctness labels. They are counted as executed, not correct. The 400 local cases were checked through the release HTTP API; the 311 additional local labels were checked against the successful local test trace. The parser implementation did not change in this follow-up.

The primary cohort deliberately includes difficult new regression cases. **43/57 is not an estimate that production titles are correct only 75% of the time.** Similarly, the combined 97.31% measures only asserted fields, not every returned field. Existing tests favor coordinates and safety guards: the primary cohort checks 184 exact season arrays and 160 episode arrays, but only 57 exact root titles. The release-300 test mainly asserts year/coordinates and nonempty titles, leaving room to strengthen independently labeled canonical-title coverage.

All 44 failed primary assertions are accounted for:

| Failure cluster | Cases | Failed assertions |
| --- | ---: | ---: |
| Newer anime-part / hierarchy candidate fields absent from old deployed version | 6 | 25 |
| Unicode-title slash dates split into path components | 3 | 8 |
| Unclosed annotations swallow descendant directory separators | 4 | 4 |
| One-letter parent titles replaced by a technical filename | 3 | 3 |
| Unicode or vertical-tab whitespace not normalized | 4 | 4 |

Whole-response comparisons for the additional 805 inputs measure **version parity**, not independent ground truth; 311 of these same inputs also supply the 722 explicit source assertions scored above. Exact whole-response equality held for 331/805. In 466 differing cases the new `anime_local_part_candidate` field explains the difference; after excluding that field recursively, equality holds for 797/805. The remaining differences are seven `edition` fields and one hierarchy candidate. Core title/year/season/episode outputs agree across this additional cohort. Learned-rule/configuration differences were not independently isolated for the technical edition fields, so they are not classified as wrong.

## Matcher results

The matcher corpus contains **383 cases**: 300 ordinary generated variants across 30 works, 78 additional scored source expectations, and five ambiguous diagnostic cases without a reliable expected identity. All 51 distinct positive IMDb identities were independently corroborated through Wikidata P345; an initial Food Wars label was corrected to `tt4731072` before scoring. This corroborates identity references, not user intent or season/episode mappings. AniDB coordinate expectations come from documented crosswalk regressions, not from Wikidata.

| Measurement | Deployed API |
| --- | ---: |
| Ordinary movie/TV/anime variants | 300/300 exact SID expectations |
| Additional difficult expectations | 66/78 agree on all labeled endpoints |
| All scored cases | 366/378 (96.83%) agree on all labeled endpoints |
| Expected-positive exact SID endpoints | 363/375 |
| Correct expected SID abstentions | 3/3 |
| Positive IMDb identity agreement, ignoring episode coordinates | 366/375 |
| Positive exact ASID endpoint expectations | 12/20 |
| Expected ASID abstentions | 20/21 |

These are small, correlated regression cohorts. Five ambiguous cases and unasserted ASID fields are excluded from the corresponding measurements. Twelve SID disagreements consist of **two different work IDs, three correct work IDs with incorrect or missing coordinates, and seven abstentions**. One different-work case is deliberately ambiguous Supernatural; it is not evidence that the service can infer which adaptation a user intended. Avoid turning these counts into a general precision claim.

### Causes verified against the deployed API

1. **JoJo S02E37 identity selection.** `JoJos Bizarre Adventure S02E37 DUBBED XviD-AFG` selected the 1993 OVA `tt0248119:2:37`, with ASID `789:37`, despite the modern TV identity being a candidate. Supplying year 2012 selects `tt2359704:2:37`. An independently verified IMDb episode must take precedence over a competing identity supported only by an open-ended crosswalk. The regression now models the competing OVA default and a missing modern S02 crosswalk instead of supplying a perfect mapping for every candidate.
2. **Stone Ocean exact alias rejection.** All three actual `/alias` responses confirm direct links to `tt2359704`; official English AniDB aliases use ``JoJo`s``. The compact keys match, but strict word-token checks did not. The fix aligns straight, curly and backtick apostrophes at query time and keeps all existing title-boundary and exact-alias requirements. The source fixture now uses real colon/parenthesis spellings and part offsets rather than idealized apostrophe aliases. No index schema change or new fuzzy fallback is introduced.
3. **Supernatural ambiguity in documentation.** `Supernatural.S01E01-E22.1080p` selected the 2011 anime `tt1679482`, consistent with the documented exact AniDB priority, rather than the 2005 series shown in the README. Adding year **2005** selects `tt0460681:1:1`–`tt0460681:1:22`. The examples now include the production year; the intentional matching policy is retained.
4. **Unicode inputs on the older deployment.** Decomposed-accent Amélie and a curly-apostrophe JoJo title were unmatched. The prior PR contains the relevant normalization fixes; those local assertions pass. A new deployment is needed before measuring the effect on the remote catalog.

Eight of the 12 difficult mismatches are Stone Ocean named-part cases, encompassing missing coordinates, wrong global season, or abstention. Fixing alias rejection addresses a proven cause, but no production recall gain is claimed before deployment and retesting.

The final release binary was also replayed through HTTP with the **actual filtered animeApi and Anime-Lists rows**, imported through the normal CLI, and the official English aliases captured from `/alias`. Controlled IMDb title and episode rows supplied the independently known series coordinates. **All nine targeted cases passed:** JoJo S02E37 retained the TV SID with a null ASID, and all eight Stone Ocean cases returned the expected part/season/ASID endpoints. This confirms the fixes against the upstream spellings and importer behavior; it is not a rerun against the complete production catalog.

### Remaining accuracy work

- **Trakt versus TVDB numbering:** the actual animeApi feed assigns Stardust Crusaders and Egypt Trakt seasons 2 and 3, while Anime-Lists maps both into TVDB season 2 with Egypt offset 24. Existing idealized fixtures used zero Trakt seasons. This can explain a missing Egypt ASID even after selecting the correct IMDb series. Reconciling the numbering systems needs independent IMDb episode evidence; the safer null-ASID result is retained rather than guessing.
- **Discontinuous lists:** `Episode 11 & 13` is still treated as an interval including episode 12. Define sparse-list behavior across parser and four-endpoint API contracts.
- **Full multilingual retrieval:** fully non-Latin titles can lose their search key even when parsing succeeds; this needs a planned index migration.
- **Canonical-title test quality:** add held-out labels for complete titles and metadata collision cases. Vary punctuation from real upstream feeds, not just filename separators. Keep ambiguous adaptation intent separate from demonstrably invalid episode coordinates.

## Reproduction

```bash
cargo build --release --locked -p matchlab-server
python3 scripts/diagnostics/benchmark_startup.py \
  --output-dir /tmp/matchlab-startup
python3 scripts/diagnostics/benchmark_startup.py \
  --first-request match --sizes 100000 --restarts 5 \
  --output-dir /tmp/matchlab-startup-match-first
python3 scripts/diagnostics/collect_title_tests.py \
  --output-dir /tmp/matchlab-title-corpus
python3 scripts/diagnostics/audit_api.py \
  /tmp/matchlab-title-corpus/parser-title-expectations.json \
  --base-url https://MatchLab.notprod.fyi --endpoint /parse \
  --output /tmp/matchlab-parser-api.json
python3 scripts/diagnostics/audit_api.py \
  scripts/diagnostics/fixtures/matcher_titles.json \
  --base-url https://MatchLab.notprod.fyi --endpoint /match \
  --output /tmp/matchlab-matcher-api.json
```

See [title test collection](../scripts/diagnostics/TITLE_TEST_COLLECTION.md) for the isolated runtime-trace workflow and conservative extraction limits. The checked-in matcher corpus retains historical source expectations and notes about ambiguous or non-strict cases; its classifications deliberately do not imply universal identity ground truth.

The [machine-readable measurement summary](measurements/startup_accuracy_2026-11-14.json) accompanies this report. Full raw response reports, trace JSONL and process logs were retained as local audit artifacts, not committed as several megabytes of repeated headers and synthetic outputs. No remote restart, configuration change, metadata refresh or deployment was performed.

## Final validation

- `RUSTFLAGS="-D warnings" cargo test --workspace --locked -j 1`: **498 passed**.
- Native regex auditor example: **5 passed**, for **503 Rust tests** overall.
- Python discovery suite: **99 passed**, including conservative corpus-extraction and HTTP-auditor tests.
- All Rust targets checked and optimized release binary built with `--locked` and compiler warnings denied.
- **26 startup scenarios** and the **9-case primary-metadata HTTP replay** passed.
- `git diff --check` passed. Docker and native ARM64 validation remain with the PR's CI runners.
