# Machine Learning and Community Learning

MatchLab parses torrent release names and matches movies, TV shows and anime against local metadata. Its **optional Machine Learning (ML)** feature helps the parser recognize more release-name patterns.

**The learner changes regex rules, never parsed field values.** For example, if a new release format contains an episode marker that the parser misses, the learner may propose a regex for that marker. Once the rule is sufficiently verified, MatchLab itself uses the regex to extract fields. The existing IMDb/AniDB matcher is still responsible for identifying and validating the result.

## Start here

MatchLab has three independent parts:

1. **Parser:** Fast Rust code that extracts release fields using its own regex handlers and transformations.
2. **Matcher:** Tantivy title search, IMDb/AniDB metadata and verified episode mappings.
3. **Optional learner:** A background Rust FTRL-Proximal model and rule validators. They observe new release patterns and produce regex additions or replacement proposals.

The model is **not called during normal HTTP matching**. The parser runs compiled regex; the matcher still decides whether a title or episode is valid. Local ML needs no GPU, Python service, Ollama, ONNX Runtime or other AI server.

There are two opt-in switches:

| Mode | Setting | What it does |
| --- | --- | --- |
| Local learning | `MATCHLAB_ML_SIZE_GB=10` | Enables learner and sizes local evidence/cache/queue with one 10 GiB budget |
| Community sharing | `MATCHLAB_COMMUNITY_LEARNING_ENABLED=true` plus a GitHub token | Sends eligible regex proposals to GitHub for review |

Both are disabled by default. Local learning works without community sharing.

## What learning actually changes

The background worker:

1. Reads new release names and **reparses them with the original MatchLab parser**, not an ML-modified or cached result.
2. Observes **every field returned by the canonical parser**, including title, year, seasons, episodes, audio, languages, release group, network, edition and other metadata. The background audit captures **four error classes**: missing extraction, excessive extraction, incomplete extraction, and incorrect field assignment (such as swapping `seasons` and `episodes`). It also records explicit markers that contradict nonempty parser values.
3. Trains a small online FTRL-Proximal ranker to prioritize field-specific regex improvements. Its score is **not a probability of correctness**.
4. Builds candidate regex from restricted, linear-time grammars. Arbitrary model-written PCRE and downloaded code are not executed.
5. Requires repeated examples from independent titles, conflict checks and negative tests before a regex can be promoted. **Every field may be considered**, but an unverified pattern stays inactive regardless of the field.
6. Compiles validated additions and source-handler replacements **on the background worker** and atomically installs a new parser engine. Existing parser field names, transformations and matching logic are not modified by ML.

A promoted rule can be **additional** (recognize a new syntax) or a **replacement** for an existing regex. Replacement patterns preserve the original parser handler and its transformation; source changes retain the original matching branch when safely possible. When proof is insufficient, the proposed regex remains review-only.

### All parser fields are learning targets

Learning is **not limited to video tags or TV coordinates**. The background model receives every populated canonical parser field, and the discovery process can rank unfamiliar markers against the original parser's complete field catalogue. Example targets include:

| Field family | What regex improvement may address |
| --- | --- |
| Title, year, date, season, episode and episode code | Better identification of structural boundaries and episode/year notations |
| Languages, subtitles, dubbing, audio, codec and channels | More accurate tagging of language and sound properties |
| Resolution, quality, bit depth, HDR and other video properties | Recognition of new or alternate release markers |
| Network, release group, edition, region, release type and other flags | Better extraction of release and distribution metadata |
| Anime episode naming | Additional candidate regex, with independent IMDb/AniDB episode verification by MatchLab |

**Every field can contribute evidence and become the subject of a proposed regex improvement.** Automatic live activation still requires a dedicated validator for the pattern and field, enough independent examples, negative checks and safe parser behavior. A field with no sufficiently validated rule remains in learning/review; it is **not** rewritten or guessed by the model. The current implementation has live validators for selected technical and explicit TV patterns; expanding automatic verification to other field families remains part of the production-readiness work. No field is permanently excluded from learning.

**No ML method writes `seasons`, `episodes`, `codec`, `title`, `year`, SID or ASID into a parser response.** MatchLab creates those values using its own original or learned regex rules, transforms and independently verified metadata.

### Evidence and safety

Current local rule-promotion gates include:

- Technical fields: at least **8 agreeing observations from 4 title identities**, including **4 missing-field observations from 2 identities**, to qualify for a native regex addition. Native successes remain training evidence but cannot activate a duplicate regex. A corresponding native Rust handler must actually exist for the field and transform; unsupported live fields remain review-only.
- Selected source-regex replacements: at least **16 observations, 6 identities and 2 original-parser confirmations**.
- TV coordinate patterns: at least **8 observations from 4 title identities**, **2 canonical confirmations**, and **3 explicitly missing season/episode observations across 2 identities**. Disagreements, packs and ambiguous coordinates are not considered correctable missing-field evidence. Previously trained TV evidence is archived; the new evidence epoch retrains this decision from fresh raw titles.
- Explicit TV conventions: at least **8 observations, 4 title identities and 2 original-parser confirmations**.
- Absolute anime markers: at least **8 independently verified examples**, including diversity across IMDb identities and absolute episodes, plus an independently checked AniDB/TVDB crosswalk.

These counts are conservative **engineering thresholds**, not statistically calibrated confidence values. Conflicting markers, ambiguous episode ranges, changes to parser builds, or invalid source mappings must block promotion. Incompatible historical evidence is retained for review or pruned under quota pressure; it cannot silently authorize current rules.

For source handlers, MatchLab knows the existing regex catalogue. The learner may submit the **original regex and proposed replacement regex**, with handler identity and supporting evidence. Live changes are limited to safe, verified families; riskier suggestions go to the GitHub review queue.

## Long-term goal: every parser field

Every field exposed by MatchLab's original parser catalogue must participate
in learning and be eligible for ADD, REDUCE, EXPAND and CORRECT proposals.
Activation still requires **independent reference evidence** for the field
and replay of the entire parser, not a high ML score alone.

Unambiguous technical markers (resolution, codec, audio, HDR, channels,
named languages, subtitle markers and extensions) can use verified native
Rust handler transformations when a field-specific reference is available.
Context-dependent fields such as title, year, release group, network and
edition must obtain an independent reference (for example independently
verified metadata or a curated labelled corpus) before replacements are
allowed to become active.

This is a **verification requirement, not a permanent product restriction**.
The objective is not a TV-only learner, nor a model that amplifies its own
mistakes by treating the parser's past output as ground truth.

## Four-error corrective regex learning (experimental)

The audit distinguishes **missing**, **overextracted**, **underextracted** and
**misassigned/incorrect** parser fields. All fields remain visible to the
advisory FTRL model. Missing technical markers continue through the ADD
learner. A separate corrective learner now records independently readable,
single-release **TV season/episode** evidence:

- **REDUCE / CORRECT:** A release with an unambiguous SxxEyy, NxM, or
  "Season N Episode M" marker can identify excessive episode values,
  swapped fields, or incorrect nonempty coordinates. The learner can
  propose *high-priority native-handler regexes* for both coordinates;
  unlike ordinary appended rules they may prevent incorrect earlier
  handlers from winning.
- **EXPAND:** A bounded, explicitly marked SxxEyy-Ezz interval is checked
  against the entire extracted episode array. A native \`range_func\`
  handler can be proposed ahead of existing handlers when the original
  parser extracted only part of the interval.
- **ADD:** Missing technical markers and missing single TV coordinates
  remain handled by existing additive evidence gates.

### Rule lifecycle: add, replace, reject, retain

A candidate is not installed because it looks like a plausible regex or
collects many repetitions. The native parser must replay a bounded local
corpus with **the entire already-installed handler catalogue and learned
technical/TV additions**. Before publication, the worker confirms that:

- At least four independently observed parser errors are **actually repaired**
  and every candidate field matches an independently recognizable syntax.
- Correctly parsed releases retain their **entire** original result, including
  episode ranges, title boundaries and unrelated metadata fields.
- Other TV regex families remain correct **when all accepted corrections are
  compiled together**; a safe isolated regex can still fail the combined test.
- A rule that produces **identical parse results** is a no-op: it stays in
  local training evidence but is not a second active regex or a cache change.
- Learned additions are deduplicated by native field, regex and transform.
  A high-priority correction never also gets appended as a lower-priority
  duplicate. When the exact existing native handler is reusable, it is
  repositioned instead of copied.
- Active corrections and observed repair counts are materialized in local
  SQLite. After a parser upgrade, historical raw release evidence is reparsed
  against the new canonical parser before any previously learned regex can
  be activated again. A previously useful regex retires automatically if
  the new parser already solves the cases it addressed.

The learner therefore performs **ADD** for genuinely new patterns,
**REPLACE/reprioritize** where independently safe, and **REJECT** for no-ops,
ambiguous cases or regressions. It does not accumulate redundant rules.
Current live corrections remain narrowly scoped to verifiable TV grammars;
unverified suggestions for other parser fields remain review-only.

Corrective rules **do not directly set parsed field values**. They reuse
the original Rust parser's existing field, transform and option semantics.
The proposed regex must pass at least eight examples from four distinct
title identities, including four errors across two identities and two
canonical confirmations. The worker compiles the candidate privately,
replays up to 100 locally stored releases plus negative controls and rejects
it if previously correct values, title boundaries, or unrelated fields
regress. Failed proposals are kept for review; no live engine changes.

The local corrective corpus stores raw release names in the **local ML
database only** because replay requires original input. The corpus is
bounded to 1,024 examples; neither status responses nor GitHub community
proposals include raw titles. Community proposals contain only allowlisted
regex text, source handler index, observation counts and failure classes.

**Safety boundary:** Only the independently verifiable TV grammars above
have live corrective validators at present. Suspicious title boundaries,
technical mismatches, ambiguous packs and other fields are observed but
remain review-only until they gain independent validators. The existing
technical source-handler broadening still preserves its old regex branch;
that behavior alone does not fix false-positive extraction. A model score
is never sufficient to enable a correction, and neither parser/matcher IDs
nor season/episode numbers are written directly by ML.

## Enable local learning

Enable local learning with a single GiB budget in the Compose service:

```yaml
environment:
  MATCHLAB_ML_SIZE_GB: "10"
```

A value of `0` (default) disables ML; `1`–`128` enables it and sizes the
training evidence, result cache, queue and worker buffers. The 10 GiB example
derives up to 500,000 training observations, 500,000 cached entries and
900,000 pending queue items. These are ceilings, not guaranteed retained rows;
real SQLite disk usage can reduce them. See [ML scaling](ML_SCALING.md).

The model, verified evidence and active rules persist in `/data/local-learning.sqlite3`. A separate bounded durable queue in `/data/history.sqlite3` stores post-match observations, independently of the dashboard's 1,000 detailed rows. Cached parse/match results are stored in `/data/ml-result-cache.sqlite3` (configurable with `MATCHLAB_ML_CACHE_STATE_FILE`), **not** alongside ML training writes. Old cache rows in the previous learning SQLite file are not migrated and never used as labels; it is safe to let the bounded cache rebuild. The worker publishes new compiled rules without restarting the server.

`GET /api/ml/status` provides counts, model diagnostics and active rule summaries, without publishing original release names.

### Novelty, negative controls and active-rule accounting

The offline learner reparses every queued raw release through the **canonical native parser without learned overlays**. It records *correctly parsed* field markers separately from *independently witnessed missing fields*. Both can train the bounded FTRL model; only true missing-field gaps can authorize a live regex. False-positive or conflicting field values do not count as missing evidence.

Only technical fields with a known, applicable native Rust handler are counted as active field regex rules. Extension, language, subtitle and other non-executable candidates remain as evidence/review proposals. Negative controls reject ambiguous release markers, conflicting tags, title words mistaken for technical tags, season/episode packs and conflicting coordinates. Candidate patterns are assembled from an allowlisted finite grammar, not executable model-generated arbitrary regular expressions.

Previously archived rules remain in `ml_shared_rule_archive` for review and sharing. Lower active counts after upgrading are expected: this is deliberately stricter live activation rather than loss of historical evidence. Status includes `evidence_only_candidates` and `activation_policy`, separate from the number of compiled patterns.

### Regex logging and SQLite contention

When local learning is active, MatchLab logs each currently active custom regex on startup and logs each new, modified or retired regex during operation. The `custom ML regex inventory` event includes `active_custom_rules`, `field_rules_in_database`, `anime_rules_in_database`, `tv_rule_families`, `compiled_regex_patterns`, and `archived_custom_rules_in_database`. Only rule identifiers and regex patterns are logged, never original torrent filenames or the private training dataset. Set `RUST_LOG=matchlab_server=info` to display these events.

The history writer reuses one SQLite connection and commits up to 64 history events together while keeping 14-day minute statistics and 1,000 raw history records. The ML observation queue persists separately from these 1,000 raw history records and is consumed in bounded batches using an atomic training cursor, idempotent retry and exponential backoff. Optional cached results use a separate bounded, asynchronous SQLite writer rather than a write connection per public request; cache saturation skips cache storage but does not drop the matching result. Check `/api/ml/status` for the number of cache events skipped. A temporarily busy learning database cannot be allowed to interrupt public matching.

### Cache and upgrades

The optional cache stores exact-key results in its own SQLite database and keeps a bounded 32 MiB in-memory hot tier. HTTP requests **never open SQLite to read or write cache entries**; they use the hot tier and enqueue best-effort persistence. A cold cache miss is parsed/matched normally; the recent durable entries are rehydrated asynchronously after restart. The cache does **not** perform IMDb or title search. Tantivy remains responsible for normal title lookup.

A new parser build, active regex change or metadata generation changes the cache namespace. **An obsolete completed parse or match is never reused.** The next request is parsed and matched again with the current code and rules. Training evidence can be retained, but cached answers and previously learned outputs do not become training truth.

Successful results and no-matches have different expiration times. A single
`MATCHLAB_ML_SIZE_GB` budget is allocated as 50% evidence, 30% cache and
20% queue/history. SQLite page-accounting enforces the evidence and cache
soft limits; a trigger-maintained queue counter tracks actual rows and
estimated byte usage so queue pruning does not require deep `OFFSET` scans.
Neither allocation constitutes a strict OS filesystem quota, particularly
while SQLite WAL/checkpoint files are present. Queue overflow may discard
optional observations, never a public match response. Training transaction
size scales automatically (up to 128), and failed jobs retry without
advancing the committed cursor. Normal work never waits for ML writer locks. The approach follows [StremThru's bounded persistent job queue](https://github.com/MunifTanjim/stremthru/tree/main/internal/job/job_queue): identity/dedup, bounded batches, delayed retry and independent workers.

## Share regex improvements through GitHub

Sharing is always optional. Provide a GitHub token that can comment on Issues in the upstream repository, and configure:

```yaml
environment:
  MATCHLAB_ML_SIZE_GB: "10"
  MATCHLAB_COMMUNITY_LEARNING_ENABLED: "true"
  MATCHLAB_COMMUNITY_GITHUB_TOKEN: ${MATCHLAB_COMMUNITY_GITHUB_TOKEN}
```

Store the token in a private `.env` file or secret store, **never in a committed Compose file**. For a complete step-by-step token creation and Docker Compose setup, follow [the README sharing guide](../README.md#share-improvements-with-the-community-optional).

Use a **fine-grained PAT** restricted to `Dizzybog/MatchLab` with `Issues: Read and write` when the upstream repository is available in your token's repository selector (for example as its owner). External contributors who cannot select upstream may use a **classic PAT** with the `public_repo` scope, but it permits wider access across public repositories; set an expiration and revoke when no longer needed. `repo` (private-repository scope), Contents write, PR write and Actions write are not required.

At startup, when both ML and community sharing are enabled, MatchLab calls GitHub's authenticated user and repository endpoints **without publishing a test comment**. The log confirms `community sharing GitHub token valid: authenticated and upstream Issues reachable` after successful authentication. `GET /api/community/status` includes `token_authentication` (`not_checked`, `authenticated`, `rejected`, `repository_unavailable` or `network_unavailable`) and `ready` is only true after verification. Verification is retried every five minutes, and neither the token nor the response payload is printed. This check cannot prove Issues comment *write* authorization; actual posting permission is confirmed only by a successful HTTP 201 response. For HTTP 403 when posting, revisit PAT scopes, expiration and repository interaction restrictions.

A participating installation sends at most **one confirmed batch comment every 24 hours**, containing up to 16 items (maximum 12 regex proposals). The first eligible batch can be sent immediately when sharing is enabled; later confirmed deliveries are separated by 24 hours. The shared daily GitHub Issue collects bounded reports and regex proposals. The source-regex proposal includes the old and new regex, its handler metadata, and aggregate evidence.

### Offline archive, historical backfill and confirmed delivery

- Local learning saves an append/update-by-fingerprint archive of eligible and activated regex in `/data/local-learning.sqlite3` **even with sharing disabled**. Retiring a rule or upgrading the parser does not delete the archive. The archive starts recording when this feature is installed; previously deleted rules cannot be recovered.
- Turning GitHub sharing on exports the entire archive over successive bounded daily batches. Current rules must pass the latest independent validation. Older or lower-evidence rules become `historical_regex_review` items, strictly **non-executable** until human review and new independent validation.
- Pending and confirmed fingerprints persist in `/data/community-learning.sqlite3`. A delivery is recorded only on a GitHub HTTP 201 response or when the same batch ID is found in a recent official community Issue. A failed or ambiguous response leaves the batch retryable.
- GitHub Actions deduplicates against its global fingerprint ledger, existing accepted rules and a persistent unprocessed-proposal backlog maintained as a GitHub Release asset. Review throughput is intentionally bounded; receiving a comment is **not** proof that a rule has passed independent screening or been merged.
- A batch can contain reports and rules observed by **older builds** than the version sending the batch. The receiver checks that every source SHA is an actual Git **ancestor** of the batch SHA (itself an ancestor of trusted `main`), enforces the exact schema epoch and verifies the original full batch fingerprint list *before* inspecting individual entries. Invalid children are skipped without discarding correctly signed, independently reviewable siblings. None of these reports becomes a trusted label; all observations are reparsed with the current Rust parser.
- A community PR is created only if the accepted rule JSON changes **semantically**. Formatting an empty `[]` differently must never create a review PR, advance a rule fingerprint or consume the pending review backlog. Historical rules and technical source patches are still review-only until separately tested.
- `GET /api/community/status` provides local archive, pending and confirmed-delivery counts. A remote global duplicate is decided by GitHub; local status cannot assert whether a remote PR is merged.

The trusted GitHub Action:

1. Validates incoming data, build ancestry, deduplication fingerprints and allowed regex grammar.
2. Replays relevant samples against the current Rust parser when the evidence is available.
3. Creates at most one **human-review PR** for the daily run, and **only** for a genuine rule-data change. A compiler/serializer rewrite of empty JSON is a no-op.
4. Keeps technical, TV and source-handler suggestions in `learning/community_regex_proposals.json` as **non-executable review data**. Independently approved changes must be integrated before they affect other installations.
5. Publishes independently reviewed anime marker rules through `learning/community_rules.json`.

**No community submission automatically replaces MatchLab source code, activates an unverified regex on another server, or merges its own PR.**

Some reports can contain sanitized filename samples. **Unverified regex discovery proposals may also include the literal unknown release-marker token and its candidate regex** for human review. These tokens are never included in the local status endpoint. Removing paths, credentials and URLs does not guarantee a filename or marker is non-sensitive. Only enable optional GitHub sharing if you consent to publishing this information.

### Maintainer setup

The repository needs Issues, GitHub Actions and Releases enabled. Under repository Actions settings, allow workflows to create pull requests. The scheduled workflow runs on the trusted default branch; it requires a controlled post-merge Issue → intake → review-PR test before declaring community sharing production-verified.

Maintainers should independently validate representative IMDb/AniDB mappings, negative examples, field-level regressions and every proposed original-handler replacement before merging. The GitHub token supplied to a contributor is not trusted attestation.

## Performance and reliability checks

The architecture follows research on [FTRL-Proximal](https://research.google/pubs/ad-click-prediction-a-view-from-the-trenches/) for compact online learning and [weak supervision](https://pmc.ncbi.nlm.nih.gov/articles/PMC5951191/) for cautious treatment of correlated labels. [Rust regex and RegexSet](https://docs.rs/regex/latest/regex/) provide a bounded matching strategy; the existing parser also has trusted PCRE2 handlers, so measure the complete engine.

Production sign-off requires:

- A manually verified, title-separated **and time-separated** holdout dataset.
- Precision, recall, false positives, abstentions and overwrites measured **by parsed field**; the model's pairwise score alone is not overall parser accuracy.
- ARM64 and AMD64 comparisons of `main` versus ML disabled/enabled, including p50/p95/p99 latency, CPU, RSS, SQLite writes and disk growth.
- Concurrent live regex changes, rollback, cache invalidation and metadata-refresh tests.
- Verification that bad proposals **fail closed** and do not disrupt standard matching.
- Full Rust workspace tests, security intake tests and multi-platform Docker builds on the **final merged code**.

A larger model or additional inference runtime is not justified until realistic measurements identify a benefit. Enabling additional fields for live regex promotion must be preceded by field-specific validation tests, not merely high ML scores.

## Troubleshooting

If learning is disabled, the normal parser and matcher operate without an ML worker. If learning fails or its database is unavailable, normal matching must continue with the last safe compiled parser. Insufficient evidence cannot activate new regex. Changed builds or metadata require revalidation; an old learned rule must not silently become active.

If sharing is enabled but the token, official build identity or local learning is missing, community reporting stays disabled. Check `GET /api/community/status` and container logs.

For endpoint details see [API reference](API.md); for parser semantics see [Release parser audit](RELEASE_PARSER_AUDIT.md); for metadata see [Metadata lifecycle](LOCAL_METADATA.md).
