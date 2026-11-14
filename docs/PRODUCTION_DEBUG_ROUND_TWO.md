# Second authenticated parser and matcher review

Date: 2026-11-14. This review follows [the first protected-feed review](PRODUCTION_DEBUG_REVIEW.md) and starts from merged PR #145, commit `fd02292468865b1c5bb5c491b39b3e61d74e8b8a`.

## Fresh evidence and version boundaries

A new bounded read of the protected debug API captured **801 distinct history records and 795 unique request strings**, with no exact request-string overlap with the previous 658-record capture. Request timestamps span `2026-11-14T20:24:55.436Z` through `20:25:30.542Z`; the nine-page capture completed at `20:25:38Z`. There were 302 recorded matches and 499 unmatched requests. All recorded source policies were `public_api`.

The reported deployed build was still `dfe78a5b67c08dddff935c589e29e300b8728565`, seven commits behind the merged baseline. Existing `.ts`, publisher, banner, release-group-year and connective fixes are already present locally. Old deployed outputs are therefore **leads to investigate, not evidence that the merged fixes failed**.

Every captured title was replayed through the saved release binary containing all merged fixes. All 801 local `/parse` requests returned HTTP 200. Compared with stored match diagnostics, 764 objects agreed exactly, 20 differed only by the server's `anime_title_verified` marker, three included additional verified server title/range overlays, and 14 had other native output differences. Server overlays are not parser regressions; successful HTTP execution is not a correctness label.

The old service initially reported 1,000 retained records. Concurrent arrivals and pruning continued between pages, so this is a rolling, non-exhaustive sample. All 801 original `year` and `series` overrides remain unknown. Request strings are not independent work identities. Neither the match fraction nor the 188 heuristically flagged rows establishes precision, recall or an overall accuracy score.

## New confirmed parser behavior

Six independently reproduced defects affect **11 captured rows** in the current merged parser:

| Defect | Captured rows | Correction and preservation controls |
| --- | ---: | --- |
| Collection year protection clears explicit seasons | 3 | Preserve independently labelled `S01`, `Season 1`, `S01-06` and localized season evidence. Generic film collections still have no invented season or episode. |
| The short iTunes tag `iT` consumes the title word `It` | 3 | Require a technical boundary or explicit tag context for this ambiguous short token. Keep ordinary `iT`/`iTunes` release tags. |
| Removing a date joins neighboring title and technical tokens | 2 | Remove the captured date rather than its surrounding delimiters, so a following `HDTV` remains recognizable. Date fields and title boundaries remain intact. |
| Plural `10Bits` is not recognized as bit depth | 1 | Preserve native bit-depth conversion for singular and plural 8/10/12-bit forms; do not synthesize an episode from the number. |
| Joined `TrueHD7.1` loses channel/audio evidence | 1 | Admit the joined TrueHD channel boundary while preserving separated 7.1 and existing 5.1/2.0 forms. |
| A release-group name beginning with `HDMa` blocks plain DTS | 1 | Require a complete lossless marker before excluding DTS; preserve DTS-HD MA, Master, X and HR variants and the release-group field. |

Six synthetic regression tests include positive examples and preservation controls. Date deletion keeps its original lexical title boundary while removing only the actual date text. Contextual network lookup skips rejected title words and continues to a genuine later source tag; dated source tags remain eligible after season/episode removal. URL-banner recovery rechecks only the ambiguous iTunes value against the already computed clean release parse. Leading dates, branded show names, explicit network brackets and genuine post-title iT/AMZN tags have dedicated preservation controls. Runtime pattern corrections retain the generated catalog's source identities and conversion options. Two contradictory-codec requests were left unchanged because the raw input does not establish a preferred codec. Existing explicit single `SxxEyy` coordinates did not justify a general episode rewrite.

## Bilingual title matching

Some releases contain two complete catalog aliases in a terminal bracket, such as a localized title followed by its original title. Treating their concatenation as one strict alias misses a legitimate identity. Discarding arbitrary bracket contents would risk selecting a different work.

The strict matcher now accepts one terminal balanced `(...)` or `[...]` pair only when **both complete components are exact imported aliases with exactly one shared eligible IMDb identity**. Each component retains Unicode-aware words, original word counts, media type, production-year and season checks. Candidate sets are complete before their intersection is evaluated; a result limit or vote count cannot conceal a second shared identity.

Existing eligible full-title candidates take precedence, including literal parenthetical titles and ambiguous full-title shortlists. Unknown bracket text, aliases of different works, multiple shared identities, malformed/nested brackets and non-terminal annotations are rejected by this fallback. A name that is ambiguous alone can be disambiguated by a second complete alias when their eligible intersection is unique. Broad matching behavior remains unchanged; no index schema migration, fuzzy search or SQL title scan is added.

Catalog probes independently establish the two aliases for four public works: `Caballo soñador` / `Dream Horse` (`tt9883996`, 2020), `Love at Second Sight` / `Mon Inconnue` (`tt8265928`, 2019), `Ichi the Killer` / `Koroshiya 1` (`tt0296042`, 2001), and `El abismo` / `The Ledge` (`tt6805938`, 2022). Controlled fixtures also include higher-vote competitors, differing alias years/types, Unicode and word-count collisions, full literal precedence and insufficient season coverage.

Seven other prospective bilingual inputs have no independently established expected identity and remain unscored; the isolated fixture continues to abstain for all seven. Purely non-ASCII aliases with an empty legacy index key still require a separate index migration. This change does not claim to resolve that limitation.

The matcher semantic cache revision is incremented so persistent misses computed with previous parser/title rules are not reused after upgrading with otherwise identical metadata and learned rules.

## Verification and remaining limits

The new corrections are measured against the saved **merged baseline**, rather than the older deployed service. The primary private parser cohort has **11 actual affected inputs and 45 independently specified raw-syntax assertions**. The baseline passes 30 individual assertions and no complete case; the corrected parser passes **45/45 assertions and 11/11 complete cases**. These known failures are deliberately selected, not a random accuracy sample.

Replaying all **801 titles** succeeds before and after the change. **14 rows change**: the 11 primary cases plus three separately reviewed corrections (one false TeleSync value removed and two explicit release groups recovered). The remaining 787 parser objects agree exactly. All additional differences were inspected against the raw syntax; unchanged outputs are not assumed correct.

The same isolated alias fixture and index, strict mode and disabled learning are used for both matcher binaries. Four catalog-backed actual inputs improve from **0/4 to 4/4 expected IDs**; **14/14 sanitized positive and preservation controls** pass (12/14 in the baseline). Seven unlabeled prospective cases remain abstentions. Fixture coverage and unknown original overrides limit these results to the stated checks.

Final validation:

- Workspace Rust tests: **526 passed**; regex auditor: **5 passed** (**531 overall**).
- Python discovery: **99 passed**.
- All-targets Cargo check and optimized release build with `--locked` and compiler warnings denied.
- The private parser cohort, complete 801-input execution and controlled matcher replay checked against the final release binary.
- Source hashes and binary hashes recorded in the measurement artifact; whitespace checks run before publication. Docker and native ARM64 validation remain with PR CI.

Raw production filenames, paths, publisher labels, debug records and credentials remain outside Git. Committed regression inputs use synthetic release shapes, with public work names only where exact imported aliases are necessary to verify a matcher rule.

The [sanitized measurements](measurements/production_debug_round_two_2026-11-14.json) record the capture boundaries, version distinctions, binary hashes and final verification results.

Graphify's impact analysis used the current `fd022924` graph to map `diagnose_with_policy` and `search_inner_mode` callers and dependencies. Its bounded graph result is not a completeness or runtime test claim. Current source and executable regressions govern the final change.

No remote deployment, configuration, metadata or retained history was edited. Bounded public catalog and match probes can enter normal history and learning queues. Local fixtures use isolated metadata and indexes with learning disabled. They do not reconstruct unknown original request overrides or prove a production-wide accuracy gain.
