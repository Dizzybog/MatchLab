# Review of 3,000 new IMDb title word-count discrepancies

Date: 2026-11-14. This review follows [the coordinate-discrepancy audit](COORDINATE_DISCREPANCY_AUDIT.md). The baseline is merged PR #147, `af27282548f8c97b210e473fbeeea906ebcdd784`, whose source tree equals its head `c3cb1db57fc7558629f2ce1e1ac26d857d9aa4dc`.

## Capture and comparison

Authenticated collection continued until **exactly 3,000 previously unanalyzed unique requests** were stored. Every accepted row is matched, has a valid selected IMDb ID, and has unequal positive word counts between its exact stored `parsed.title` and `selected.title`. The user explicitly chose the title the matcher selected. No primary IMDb title or candidate alias replaces either operand. All 3,000 selected strings also have same-ID IMDb candidate alias evidence; that proves an indexed alias, not primary-title status.

Counting uses the existing Rust `title_normalization::title_word_count` directly, through a standalone helper compiled against the current module. It retains Unicode word characters, apostrophes and combining marks; spaces, dots and hyphens separate words, and `&` counts as a connective. Whitespace counts are secondary diagnostics. Independent validation includes 19 filter/schema controls and 16 word-counter controls.

Uniqueness requires both an unseen exact UTF-8 request and an unseen NFC-normalized, case-folded basename. Exclusions cover **6,507 earlier analyzed/exercised inputs**, including the previous 3,000-title capture and controlled fixtures, with 6,265 normalized basename seeds. Independent checks confirm 3,000 unique requests, basenames and source IDs, zero prior overlap and exact agreement between the export and durable SQLite records.

Collection ran from `22:12:26Z` to `22:24:58Z`: **52,951 rows scanned**, including 44,526 matched rows, across 688 pages. There were 3,118 qualifying observations, 13 prior-input observations and 96 duplicate candidate observations. Observations remaining after the quota in the final page were not admitted. No capture failure, malformed row, reported cursor expiration or history reset occurred. Rolling retention can still create internal gaps; this is not exhaustive traffic coverage. Agents suspended remote match/debug-match probes during collection.

All 3,000 source policies are `public_api`. Original request options are known and explicitly `year:null, series:null`. Historical strict/broad mode and matcher revision are unknown for every row. The service reported `fd02292468865b1c5bb5c491b39b3e61d74e8b8a` throughout capture, through PR #145; PRs #146–147 were absent. A nearby build observation is not a per-row semantic-version stamp.

## Analysis of the complete cohort

There are **2,189 parsed titles with more words** and **811 with fewer words**, covering 2,336 IMDb IDs. These disjoint lexical shapes describe the selected population, not correctness:

| Relationship | Rows |
| --- | ---: |
| Selected name is a prefix of parsed name | 1,234 |
| Selected name is a suffix of parsed name | 631 |
| Selected name is inside parsed name | 20 |
| Parsed name is a prefix of selected name | 348 |
| Parsed name is a suffix of selected name | 162 |
| Parsed name is inside selected name | 9 |
| Neither name contains the other | 596 |
| **Total** | **3,000** |

Aliases, translations, country qualifiers, named parts and connective spelling can legitimately change word count. Minimizing this count is not the objective.

All 3,000 inputs were executed against the frozen current native parser and given individual private evidence records. Stored `Diagnostic.parsed` includes server annotations and metadata/path/coordinate overlays, whereas standalone `/parse` does not. The comparison separates those scopes:

| Stored diagnostic versus native baseline | Rows |
| --- | ---: |
| Identical objects | 2,797 |
| Server annotations only | 100 |
| Annotated server overlay fields only | 16 |
| Possible path presentation-title overlays | 28 |
| Unannotated anime coordinate suppression shapes | 7 |
| Other native/rule field differences | 52 |
| **Total** | **3,000** |

Of the final 52, 39 carry existing PR #146 signatures: title-word/network boundaries (22), bit depth (6), collection-year/season handling (9), a network/group boundary (1) and a date/network boundary (1). The other 13 remain unattributed field differences. No PR #147 native-parser signature was identified. None are automatically counted as new bugs or gains.

Native baseline word counts already agree with the frozen selected alias in 30 cases; 2,970 still differ. Twelve native title strings exactly equal the stored selected title. Agreement does not establish correct identity.

## Proposed corrections

**Parser boundaries.** Ambiguous title words such as `Nick`, `True`, `Mara`, `Mar`, `Pol`, `se`, `Russian`, `Diamond` and `Mono` no longer become metadata before established release identity boundaries. Contextual capture iteration still finds genuine tags later in the same filename. For these guarded tokens, tag-only brackets remain metadata and bracketed title prose is protected. Natural phrases such as `in Russian` are protected inside a title, while genuine post-year/quality language tags still produce `ru`.

Validated leading publisher-domain banners are removed consistently, including chained banners, without consuming fansub annotations. Language captures inside recognized banner hostnames no longer become content languages. Hyphenated titles ending in a production year retain their final title word instead of treating it as a group. Narrow handling also recognizes explicit `3D HOU` with technical evidence and removes `(Digital)` immediately before a separately parsed final publisher annotation.

The primary parser oracle contains **47 actual cases and 123 assertions**, specified from release syntax before after-change outputs: publisher boundaries (26 cases), hyphenated title/year endings (7), language words (7), editions (3), audio (2), network (1), channels (1). The baseline passes 54 assertions and no complete case. Three separate preservation cases add six assertions. Posthoc Digital examples are excluded from the primary denominator. Independent review caught and corrected an initially overbroad `in Russian` exemption; committed controls cover title phrases and genuine later language tags.

**Complete-title precedence in the matcher.** An unmarked anime episode crosswalk could promote a shorter identity over the current top complete title. The new guard anchors that complete match using compact-key equality, full Unicode words and original word count. It preserves equally complete candidates, stronger full-release recoveries and existing eligible exact-AniDB exceptions. Those exceptions retain the existing compact-key semantics in broad mode; they are not promoted to strict Unicode proof. Explicit-season and named-part mapping paths keep their existing behavior.

Ten captured requests across Dragon Ball Super, Yu-Gi-Oh! 5D's, Saint Seiya Omega and Voltes V: Legacy reproduce the identity failure in a controlled broad-mode catalog. Strict baseline already retains all ten identities. Public mappings and independent IMDb season bounds support the identity expectations; unproved episode conversions are not invented. Real Super season maxima are 14/13/19/30/55. A separate synthetic single-season-131 fixture tests compatibility only and is not asserted as canonical numbering.

**Dated identities.** A calendar release without explicit requested year no longer selects a series whose known production year is later, unless an independently resolved episode airdate supports it. The bounded guard abstains; it does not guess a replacement show. Unknown production years, explicit year overrides, verified pre-premiere specials and resolved adjacent-day dates retain their behavior. Fifty actual dated requests reproduce the unsafe future-series identity in broad mode; strict baseline already abstains. Three additional full-name future-date controls fail baseline in both modes and establish new strict-mode coverage. These local reproductions do not establish the historical server's mode.

**Replay provenance and caches.** New history events persist actual `matcher_context` (`strict_mode`, `semantic_revision`), exposed by the debug feed even without full details. Missing/malformed legacy context stays null. Decoration occurs after pristine cache storage. The matcher semantic revision changes so persisted old results cannot conceal the corrections; parser persistence already includes the build identity.

Graphify supplied the decision/caller impact map for `diagnose_with_policy`, crosswalks, airdate resolution and learned-regex boundaries. Its indexed snapshot was `09ff25f`, older than the current baseline, so current source and executed tests were authoritative. Static graph links are not runtime coverage or a review verdict.

## Final verification and limits

The optimized final release executes all **3,000 parser inputs with HTTP 200**. The primary parser cohort improves from **0/47 to 47/47 complete cases**, and **54/123 to 123/123 assertions**. All three preservation cases/six assertions and four additional independent language-boundary controls pass.

Exactly **242 parser objects change**, including 231 title strings; the other 2,758 objects agree exactly. All changed objects were reviewed, and the optimized release agrees with those reviewed outputs for every input. All season arrays remain unchanged; the sole episode change restores an explicit `E16` after rejecting a false edition token. Word-count disagreements against the frozen historical selected strings decrease from 2,970 to 2,770. That is agreement evidence, not an accuracy gain or a reason to force other aliases to the same length.

The matcher uses identical isolated catalogs for before/after runs and **156 expectations frozen before final execution**, covering 60 unique actual requests and explicit additional controls:

| Checks per mode | Strict baseline | Strict final | Broad baseline | Broad final |
| --- | ---: | ---: | ---: | ---: |
| Actual anime identity cases | 10/10 | 10/10 | 0/10 | 10/10 |
| Actual dated-identity safety cases | 50/50 | 50/50 | 0/50 | 50/50 |
| Temporal controls | 7/10 | 10/10 | 7/10 | 10/10 |
| Existing behavior controls | 3/3 | 3/3 | 3/3 | 3/3 |
| Synthetic single-season compatibility | 5/5 | 5/5 | 2/5 | 5/5 |

Overall the frozen matcher matrix improves from **87/156 to 156/156**, with no changed oracle and no failures. All six controlled service children and the parser replay services were stopped.

Validation also passes **548 workspace Rust tests plus five regex-auditor tests (553 overall)**, **99 Python tests**, all-target Cargo checking and an optimized release build. Cargo uses `--locked`, with compiler warnings denied. The final release SHA-256 is `896796ab45b7875041a44d8fc114925066d8ee9b5cf7022d6b772b9c9555d342`; per-file source fingerprints bind the result to this PR's tested code.

The [sanitized measurements](measurements/imdb_wordcount_3000_2026-11-14.json) record aggregate results and fingerprints for source, binaries and frozen private inputs. Raw requests, source IDs, publisher/path data and credentials stay outside the repository. Committed tests use synthetic release shapes and public work identities where needed to prove the mapping defect.

One existing Spanish `Cap` episode-prefix reparse still truncates a title at `se`; removing its false language inference does not resolve that title boundary. It remains explicitly deferred. Other alias, translation, regional and part-title hypotheses require independent metadata evidence before changing identities or accepting training labels.

Additional historical signals involve V and Garo identity substitutions and Devil May Cry truncation at `May`. They were not re-proven against current controlled catalogs and are not claimed fixed. Explicit-season crosswalk selection and legacy broad-mode compact-AniDB exceptions remain outside the new unmarked complete-title guard. A further Super request exhibits the same reviewed mechanism but is excluded from the frozen matcher score.

This cohort is conditioned on successful matches and unequal title lengths. It cannot establish production-wide accuracy, precision, recall or an error percentage. Isolated matcher catalogs and original request options do not reconstruct unknown historical runtime/metadata/rule state. Docker and native ARM64 validation remain with PR CI. No production deployment, configuration, metadata or retained history was changed.
