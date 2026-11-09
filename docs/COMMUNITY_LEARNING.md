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
| Local learning | `MATCHLAB_ML_ENABLED=true` | Learns and activates eligible regex locally |
| Community sharing | `MATCHLAB_COMMUNITY_LEARNING_ENABLED=true` plus a GitHub token | Sends eligible regex proposals to GitHub for review |

Both are disabled by default. Local learning works without community sharing.

## What learning actually changes

The background worker:

1. Reads new release names and **reparses them with the original MatchLab parser**, not an ML-modified or cached result.
2. Observes **every field returned by the canonical parser**, including title, year, seasons, episodes, audio, languages, release group, network, edition and other metadata. Missing-field clues and unfamiliar markers are considered across the full original parser-field catalogue.
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

- Technical fields: at least **8 agreeing observations from 4 title identities** to qualify for a native regex addition.
- Selected source-regex replacements: at least **16 observations, 6 identities and 2 original-parser confirmations**.
- Explicit TV conventions: at least **8 observations, 4 title identities and 2 original-parser confirmations**.
- Absolute anime markers: at least **8 independently verified examples**, including diversity across IMDb identities and absolute episodes, plus an independently checked AniDB/TVDB crosswalk.

These counts are conservative **engineering thresholds**, not statistically calibrated confidence values. Conflicting markers, ambiguous episode ranges, changes to parser builds, or invalid source mappings must block promotion. Incompatible historical evidence is retained for review or pruned under quota pressure; it cannot silently authorize current rules.

For source handlers, MatchLab knows the existing regex catalogue. The learner may submit the **original regex and proposed replacement regex**, with handler identity and supporting evidence. Live changes are limited to safe, verified families; riskier suggestions go to the GitHub review queue.

## Enable local learning

Add these options to your Compose service's environment:

```yaml
environment:
  MATCHLAB_ML_ENABLED: "true"
  MATCHLAB_ML_TRAINING_MAX_EXAMPLES: "500000"
  MATCHLAB_ML_MAX_STORAGE_GB: "10"
  MATCHLAB_ML_CACHE_ENABLED: "true"
  MATCHLAB_ML_CACHE_MAX_ENTRIES: "200000"
```

Only `MATCHLAB_ML_ENABLED` is required. The other values are optional limits. The example count is a maximum number of retained training **observations**, not RAM allocation or guaranteed unique torrent names.

The model, evidence, active rules and optional result cache persist in `/data/local-learning.sqlite3`. The worker publishes new compiled rules without restarting the server. Learning runs independently of normal matching.

`GET /api/ml/status` provides counts, model diagnostics and active rule summaries, without publishing original release names.

### Cache and upgrades

The optional cache stores exact-key results in SQLite; it does **not** perform IMDb or title search. Tantivy remains responsible for normal title lookup.

A new parser build, active regex change or metadata generation changes the cache namespace. **An obsolete completed parse or match is never reused.** The next request is parsed and matched again with the current code and rules. Training evidence can be retained, but cached answers and previously learned outputs do not become training truth.

Successful results and no-matches have different expiration times. The learning-storage quota is a best-effort SQLite disk budget managed in the background, not a strict OS disk quota.

## Share regex improvements through GitHub

Sharing is always optional. Provide a GitHub token that can comment on Issues in the upstream repository, and configure:

```yaml
environment:
  MATCHLAB_ML_ENABLED: "true"
  MATCHLAB_COMMUNITY_LEARNING_ENABLED: "true"
  MATCHLAB_COMMUNITY_GITHUB_TOKEN: ${MATCHLAB_COMMUNITY_GITHUB_TOKEN}
```

Store the token in a private `.env` file or secret store, **never in a committed Compose file**.

A participating installation sends at most **one batch comment every 24 hours**. The shared daily GitHub Issue collects bounded reports and regex proposals. The source-regex proposal includes the old and new regex, its handler metadata, and aggregate evidence.

The trusted GitHub Action:

1. Validates incoming data, build ancestry, deduplication fingerprints and allowed regex grammar.
2. Replays relevant samples against the current Rust parser when the evidence is available.
3. Creates at most one **human-review PR** for the daily run.
4. Keeps technical, TV and source-handler suggestions in `learning/community_regex_proposals.json` as **non-executable review data**. Independently approved changes must be integrated before they affect other installations.
5. Publishes independently reviewed anime marker rules through `learning/community_rules.json`.

**No community submission automatically replaces MatchLab source code, activates an unverified regex on another server, or merges its own PR.**

Some reports can contain sanitized filename samples. Removing paths, credentials and URLs does not guarantee a filename is non-sensitive. Read each opt-in's privacy implications before enabling it.

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
