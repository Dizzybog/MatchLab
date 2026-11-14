# Audit of 3,000 new matched coordinate disagreements

Date: 2026-11-14. This review follows [PR #146's parser/matcher review](PRODUCTION_DEBUG_ROUND_TWO.md). The baseline is merged `09ff25f5499cdf223504548c5593a3b0718cac2e`, whose source tree is identical to PR #146 head `4bdefaa6ff692a77e3c857d8d121c0f4daf813c2`.

## The requested cohort

Authenticated debug-feed polling continued until **exactly 3,000 previously unanalyzed unique title requests** had been durably stored. Every accepted record has `status=matched`, a selected object, and at least one of these season/episode disagreements:

- A parsed coordinate is present while its selected counterpart is missing.
- A selected coordinate is present while its parsed counterpart is missing.
- The selected coordinate is absent from the parser's coordinate values.

Zero is a coordinate, not an absent value. Missing/null/empty parser arrays are absent. Compact `episode_start` is considered when the episode array is empty. Multi-valued arrays containing the selected start are consistent representations. Malformed types and unmatched requests cannot fill the quota. Range-end differences alone do not qualify; their context remains available for analysis.

Uniqueness requires both an unseen exact UTF-8 request string and an unseen NFC-normalized, case-folded basename. This additionally rejects repeated filenames in different directories; it deliberately excludes some distinct parent-context inputs. Exclusions cover **3,383 previously analyzed inputs**, including both earlier captures, public API audits, executed source-test inputs and controlled fixtures. Eight additional recent source-test inputs were checked separately. Two independent validators confirm **3,000 unique request strings, basenames and source IDs**, zero prior overlap and an exact match between the export and the durable SQLite records.

Collection ran from `21:15:58Z` to `21:21:38Z`: **22,334 history rows scanned**, including 17,220 matched rows, across 295 pages. There were 3,773 qualifying observations and 766 duplicate candidate observations. After admitting the 3,000th unique input, remaining observations in the last page were not admitted. No capture failure, malformed-row quarantine or reported cursor expiration occurred. Rolling per-status retention can still create internal gaps; this is not an exhaustive traffic capture.

All recorded policies are `public_api`. Matching/debug probes were suspended throughout collection to avoid admitting our own synthetic traffic. Only metadata reads and isolated local fixtures were used during that period. The service reported deployed build `dfe78a5` throughout, predating PRs #144–#146; history does not establish a per-row build/rule/metadata version. All original optional `year` and `series` overrides remain unknown.

## Analysis of every accepted input

All **3,000 inputs** were executed against the current baseline parser and assigned a row-level evidence/provenance classification. The disjoint stored-diagnostic categories are mechanical descriptions, not correctness labels:

| Category | Inputs |
| --- | ---: |
| IMDb season-pack expansion | 1,372 |
| Missing-season inference for parsed episodes | 1,115 |
| Other pack/inferred coordinates | 299 |
| Calendar-date mappings | 96 |
| Withheld coordinates | 60 |
| Changed numeric coordinates | 58 |
| **Total** | **3,000** |

There are **2,942 missing-coordinate-only cases** and 58 cases with a numeric contradiction; 118 have a numeric difference or selected-coordinate omission requiring priority review. Many differences are legitimate season packs, season-one inference, calendar lookups or anime remapping. They are not 3,000 verified bugs, and minimizing the disagreement count would remove useful behavior.

Stored match diagnostics and native `/parse` output also differ in scope: 2,315 objects match exactly, 573 differ only by server annotations, 75 by annotated overlay fields, six by unannotated server coordinate gating, eight by path/title presentation, three by mixed overlays and other fields, and 20 by other native/runtime fields. Existing PR #145–#146 signatures appear in 14 inputs and are excluded from new-fix claims. Seventeen rows lack additional semantic context and remain explicitly unresolved.

## Confirmed corrections

1. **Numeric titles with dates:** a repeated numeric work name such as `20-20` no longer supplies false S20E20, allowing the existing date-resolution path to operate. The guard requires a valid date and no explicit episode syntax; genuine numbered releases remain supported.
2. **Film ordinals:** `Movie`/`Film` numbers in the native anime fallback are title/film-number evidence, not episode numbers. Full titles and explicit episode markers are preserved.
3. **Separated season and EP ranges:** ordinary TV inputs such as `S05 Ep11-20` retain their verified season-relative endpoints when exact identity and imported bounds establish the whole interval. A linked or partially mapped anime cannot use this fallback. `Naruto Shippuden S10 Ep197-221` remains a verified absolute conversion to S10E1–E25. Parser syntax alone is not globally relabeled.
4. **Named anime part ranges:** an explicit season equal to a named part's canonical base season keeps that global season for the complete interval. Missing or out-of-bounds episodes cannot fall through to a later local part. Actual Alicization metadata reproduces the old S03E03–E05 → S04E15–E17 error; forced local `P3` semantics remain separate.
5. **Film IDs with TV episodes:** episode-bearing anime discovery rejects positively cataloged nonseries IDs before borrowing a sibling TVDB mapping. When a full exact film alias is rejected, its evidence also blocks a weaker shorter TV alias in the other selection lane. Equally exact eligible series identities, unknown catalog coverage and ordinary film matching remain supported.
6. **Impossible scalar anime mappings:** a final mapped episode above an independently known IMDb season maximum loses its contradictory coordinates and associated crosswalk ASID. The verified IMDb identity remains, and the matcher does not guess replacement coordinates. Unknown bounds retain existing crosswalk coverage. The GATE input mapping absolute 17 to S01E29 against known S01 maximum 24 demonstrates the failure.

Strict AniDB discovery verifies Unicode words and original word counts. Broad mode retains its existing compact-key normalization; the new cross-lane film guard in that mode blocks weaker aliases using compact-exact evidence. It does not claim the same lexical proof as strict mode.

The matcher semantic cache revision changes so older persisted results cannot hide these corrections. Graphify supplied the coordinate decision/caller map, checked against current source; its static graph is not a runtime validation verdict.

## Verification and limits

The final optimized release executes **all 3,000 parser inputs successfully**. Exactly two parser objects change, both in the independently specified cohort: **0/2 → 2/2 complete cases** and **5/9 → 9/9 raw-syntax assertions**. The other 2,998 parser objects agree exactly; that equality is not a correctness label.

Controlled matcher replays use 40 expectations frozen before final execution:

| Checks | Baseline exact | Final exact |
| --- | ---: | ---: |
| Nine original affected inputs | 0/9 | 8/9 |
| Seven additional correctness/safety probes | 0/7 | 6/7 |
| Preservation controls | 19/19 | 19/19 |
| Unknown-bound compatibility controls | 5/5 | 5/5 |

The two deviations are one actual and one sanitized Stampede input. Their conservative oracle expected abstention; the recovered full title instead matches the correct bare film ID `tt9430698` in the controlled alias fixture. The original expected outputs remain unchanged. Primary metadata and Cinemeta independently establish that film identity, but the fixture supplies a combined full-title alias. The production alias endpoint confirms separate `One Piece Movie 14` and `One Piece: Stampede` aliases, not the combined spelling. These alternatives are not counted as exact passes or production recovery proof. All six ordinary-TV range targets pass, as do the known-bound GATE rejection and the explicit Alicization global-season checks.

Validation completed with **536 workspace Rust tests plus five regex-auditor tests (541 overall)**, **99 Python tests**, all-targets Cargo checking and an optimized release build. Cargo used `--locked` with compiler warnings denied. Source and binary hashes bind the local replays to the verified code; staged whitespace checks run before publication. Docker/native ARM64 validation remains with PR CI. All local replay services were stopped.

Final release replay and test measurements are recorded in the [sanitized artifact](measurements/coordinate_discrepancies_3000_2026-11-14.json). Raw history, input titles, source IDs, publisher/path data and credentials remain private. Committed tests use synthetic release shapes and public catalog identities where necessary to establish a mapping defect.

The primary parser checks were specified from raw syntax before new outputs. Matcher checks use the same isolated metadata/index for both binaries, actual primary-source mappings and independent catalog bounds, with explicit preservation controls. Title-only replays cannot reconstruct unknown original overrides or reproduce the full production metadata/rule state.

IMDb bounds missing from the deployed snapshot are not permission to relax validation. Regional numbering ambiguities, the yearless `Furious` alias collision, ambiguous literal/batch forms and unsupported language-specific range syntax remain documented uncertainty. The new scalar final guard does not validate every range or change the standalone `/episode/resolve` conversion policy; broader movie-family behavior there remains separate work.

This cohort is conditioned on successful matching with a coordinate disagreement. It cannot establish production-wide precision, recall or an accuracy percentage. No production deployment, metadata or configuration was changed, and no retained history was edited. Public probes after collection can enter the service's ordinary history/learning queues.
