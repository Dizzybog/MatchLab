# Review of authenticated production diagnostics

Date: 2026-11-14. This review uses actual retained requests from the bearer-protected parsing debug feed, following the earlier [startup and public API audit](STARTUP_AND_ACCURACY.md).

## Evidence and limits

The deployed build reported `dfe78a5b67c08dddff935c589e29e300b8728565`. A bounded, authenticated read captured **658 distinct history records**, containing **647 unique request strings**, across approximately 15 seconds of production traffic. Of these, 197 were recorded as matched and 461 as unmatched; 157 carried at least one heuristic review flag. All recorded source policies were `public_api`.

The service initially reported 1,000 retained rows. Because traffic and pruning continued during seven paginated reads, the capture is a **rolling sample, not an atomic or exhaustive snapshot**. All 658 records lack the original request's optional `year` and `series` overrides. Consequently, neither the matched fraction nor the flagged fraction establishes accuracy, and replaying only a title does not reproduce an unknown original override.

The credential was used only in the HTTPS Authorization header and kept out of source, logs and commits. Raw history, publisher domains, personal release-group labels and full original paths remain private local analysis artifacts. Committed parser regressions use minimal synthetic names and `example.org`; public series titles appear only where actual catalog aliases are necessary to establish a normalization defect.

## Confirmed parser defects

Five independently reproduced parser defects account for **30 distinct captured rows**. Expected changed fields were specified from filename syntax and manual review, not copied from a new parser result.

| Defect | Observed rows | Correction |
| --- | ---: | --- |
| Terminal `.ts` interpreted as TeleSync and low-quality `trash` | 16 | Recognize a transport-stream suffix without overriding explicit HDTV/SATRip evidence. Genuine `TS` quality tags before another extension remain supported. |
| `And.Co` inside a work's title removed as a publisher domain | 6 | Require appropriate publisher context before removing an early domain-shaped token; explicit sites and bracketed publishers still work. |
| Written-season title recovery restores a previously stripped URL banner | 3 | Apply the same complete leading-banner removal when reparsing a title prefix. |
| Leading advertisement with a bare DNS name becomes the title | 3 | Recognize bounded common-domain syntax inside complete leading annotations, retaining ordinary fansub groups. |
| Digits in a separately marked release group become a production year | 2 | Exclude years inside balanced named `-[GROUP1980]` groups, including underscore, dash and dot variants. Pure numeric year brackets and existing adjacent title/year forms retain their behavior. |

The release-group issue matters to matching: a false year from `-[TEAM1980]` can trigger the intentionally strict production-year exclusion even though the show title and episode are correct. The fix corrects year extraction rather than weakening year eligibility.

The six new parser test functions include explicit negative and preservation controls: real TeleSync quality, normal website extraction, fansub groups, valid year brackets, independent movie years, existing attached-year forms and nested release paths.

## Confirmed matcher defect

Strict matching used inconsistent connective normalization. Compact keys and semantic title words already equated written `and` with `&`, while the original-word-count check counted only the written word.

Sanitized public controls demonstrated the effect:

- `Locke.and.Key.S03.WEBRip.x265-GROUP` was unmatched; its `&` equivalent selected the catalog's `Locke & Key`, `tt3007572`.
- `Fat.Tony.And.Co.S01E01.720p.BDRip-GROUP.mkv` also suffered the parser truncation above. Its `&` equivalent selected `Fat Tony & Co`, `tt3457530`.

Ampersands now count as the connective word they already represent elsewhere in title normalization. This preserves strict word-boundary checks: `Rock and Roll` and `Rock & Roll` agree, while `Rock Roll` remains different. A higher-vote conflicting title without the connective is included in the regression fixture. The change uses existing raw aliases at query time; it does not require an index migration or add fuzzy identity fallbacks.

The 197 matched records were also inspected for episode shape errors. Fourteen distinct recorded ranges showed no reversed/mixed endpoints, malformed ASIDs or movie identities carrying series coordinates. Five high-severity coordinate alerts were valid season-zero extras, not evidence for changing episode mappings.

## Diagnostic noise and replay provenance

**68 original alert occurrences across 65 rows** were confirmed as heuristic false positives:

| Original alert cause | Occurrences |
| --- | ---: |
| A parsed year, date year or collection range treated as omitted title words | 54 |
| Valid season-zero specials treated as invalid coordinates | 5 |
| A calendar date or collection interval treated as a missing production year | 4 |
| Supported `WEB-DLRip` / `BluRay REMUX` treated as conflicting quality | 4 |
| A site-only bracketed banner treated as a missing work title | 1 |

These counts overlap other review reasons and must not be subtracted directly from the number of suspected requests. All 66 `unmatched_explicit_episode` records already retained the explicit season and episode in their parser arrays: that flag alone does not establish a parser error.

The fixes keep genuinely inconsistent quality and out-of-range coordinate controls visible. Coordinate validity follows the existing public SID bounds, including valid zero coordinates and absent values. A high but explicit TV season can still be a review hint; it is not automatically an invalid match.

New matching history records now retain the effective `year` and `series` options and their public/dashboard source policy, including when ML is disabled and when a matching error is recorded. This decoration happens **after cache storage**, preserving cached diagnostics and public match responses. The protected feed exposes `request_context_available` and a validated `replay_request`. Legacy records remain explicitly unknown rather than silently receiving default overrides.

The feed also derives its retention metadata from the actual shared history policy: **500 detailed entries per status**, up to **1,500** across matched, unmatched and error. Bounds and returned rows share a SQLite read transaction within each page. Multiple pages still represent a moving window and can have retention gaps.

## Verification and remaining uncertainty

The private regression cohort contains **30 actual affected inputs with 101 independently specified assertions**. Against the saved pre-fix local binary, 51 individual assertions passed and no case passed all its assertions. This cohort deliberately contains known failures; it is not a random sample of requests.

The final release passes **30/30 cases and 101/101 assertions** in that private parser cohort. Six controlled matcher cases improved from **3/6 to 6/6**. They use catalog-proven aliases and known episode bounds, with both binaries run against the same isolated fixture and index. These are title-only requests; they do not reconstruct the missing original overrides or establish full production accuracy.

Re-evaluating all **658 old stored diagnostics**, with their original parsed and selected fields unchanged, reduced flagged rows from **157 to 99**. The remaining seven new underextraction and two overextraction hints are explicitly unscored review items. All legacy replay contexts remained correctly unknown, and ten additional sanitized coordinate/context controls passed. This detector-only replay is separate from verification of corrected parser outputs.

Remaining cases were not converted into speculative matcher changes:

- Explicit release years can legitimately conflict with a series' original production year; the strict rule remains authoritative.
- 48 unmatched records had an empty normalized key, including 45 with only non-ASCII title letters. Full multilingual retrieval still needs a deliberate key/index migration.
- Some releases contain translated titles, alternate naming schemes, unsupported catalog content or all-bracketed layouts. These require independent labels before changing identity selection.
- The feed is a triage source, not ground truth. New deployments and real metadata must be checked again before estimating production accuracy gains.

The protected history capture preceded the sanitized public replay probes. Those ordinary probes can enter the service's normal history and learning queues; no history was edited or deleted, and no remote configuration, metadata or deployment was changed. Local replay services and generated credentials are isolated and terminated after verification.

## Validation and artifacts

- `RUSTFLAGS="-D warnings" cargo test --workspace --locked -j 1`: **516 passed**.
- Regex auditor example: **5 passed**, for **521 Rust tests** overall.
- Python test discovery: **99 passed**.
- All Rust targets checked and the optimized release binary built with `--locked` and compiler warnings denied.
- The private 30-case parser replay, six matcher controls, 658-record detector replay and ten diagnostic controls passed their stated checks.
- `git diff --check` passed. Docker and native ARM64 validation remain with PR CI.

The [sanitized measurement summary](measurements/production_debug_2026-11-14.json) records counts and binary hashes. Actual retained records and detailed replay outputs remain private local artifacts; the provided production credential is not included.
