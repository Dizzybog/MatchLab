# AniDB parser and matcher discrepancy review

Date: 2026-11-15. This review follows [the third protected-feed review](PRODUCTION_DEBUG_ROUND_THREE.md) and starts from merged PR #153, commit `a8d8f66`.

## Evidence

Two bounded reads of the protected debug feed hold **136,215 history records (141,661 unique request strings)**. **12,723 unique request strings** were matched with AniDB evidence (an ASID, a linked AniDB ID, the anime-priority lane or an anime candidate). The feed is a rolling window, so this is a sample, not a snapshot.

Unlike the earlier reviews, the matcher was replayed locally. The seven public metadata sources were imported with the baseline binary, and every AniDB-related request string was sent to two local servers that share this index: the baseline `a8d8f66` and this change. Both ran with `MATCHLAB_STRICT_MODE=false`, as production does. Recorded production outputs come from older builds and were used only to select the request strings.

A row counts as a discrepancy when the parser and the matcher disagree: a parsed episode or range without a selected coordinate, a season that differs, a SID without the ASID of a linked work, or a title that still ends in an unparsed number. These labels are heuristics, not correctness labels.

## Result

The change alters **470 of the 12,723** AniDB-related results. 379 gain an episode, 58 gain an ASID for an unchanged SID, 14 change season or episode, 10 gain a range end, 6 change IMDb identity, 1 gains a match and 2 lose a coordinate that the baseline assigned to the wrong series.

| Baseline | This change | Discrepancy class |
| ---: | ---: | --- |
| 10,956 | 11,346 | Consistent |
| 276 | 81 | Title ends with a number, no episode candidate |
| 259 | 197 | Range parsed, no range end selected |
| 185 | 131 | Episode parsed, no SID episode (not blocked) |
| 362 | 246 | Episode parsed, SID episode blocked by known bounds |
| 577 | 540 | SID episode without an ASID |
| 223 | 222 | Unmatched |

A random sample of 20,000 request strings without AniDB evidence changes in 25 rows. Every changed row in both sets was read.

## Parser changes

| Defect | Correction |
| --- | --- |
| A fansub episode written without a dash before the first tag produced no coordinate (`[Group] Title 12 [CRC].mkv`, `Title_07_(HD.720p)`, `Title!-05.mp4`, `Title 04 VOSTFR`) | It is published as an unverified anime episode candidate. Two to four digits are required, a following year excludes it, and dotted scene names, dated releases and product codes are not candidates |
| A zero-padded batch followed by a subtitle or a DVD/TV tag was expanded into an episode list, of which only the first episode was matched (`Title - 0575-0746 - Arc name`, `Title 1-101 dvdrip`) | It is an unverified range candidate |
| The range candidate's title was parsed from the prefix alone and could lose its tail (`Yu-Gi-Oh!` became `Yu-Gi`) | The complete release title is kept when it only extends the prefix title |

| The prefix-only parse read a hyphenated title tail as a release group, so the episode candidate named a different title (`Kaijuu 8-gou` became `Kaijuu 8`) | The candidate keeps the tail when the complete release title has it |
| `[Group][Title][14]` produced an episode without the fansub structure the matcher requires | It carries a bracketed absolute episode candidate |
| `Title Part 2 - 01 ~ 12` was read as season 2 episode 1 and the batch was dropped | A number followed by a batch is not a season-dash episode; the batch is a range candidate |

Replaying all 141,661 request strings through both parsers changes 1,230 results: 1,166 gain or change an episode candidate, 65 change a range candidate and 35 change `episodes`. No result loses a coordinate without gaining a candidate for it.

## Matcher changes

- **An exact alias is not a shortened name of an unrelated prefix.** The shortened-AKA penalty demoted `Dragon Ball Kai`, whose primary IMDb name is longer, below the prefix `Dragon Ball`. The penalty now applies only when another candidate carries the release title as its primary name.
- **Part-titled episodes and batches.** When several AniDB works of one IMDb series accept the same absolute number, an unmarked episode or batch resolves through the exact part alias, as the dashed form already did. A bounded AniDB work that independently proves the episode keeps priority.
- **A numbered alias proves a season only when it names that season.** `Title Part 2 - 05` was promoted to IMDb season 2 because season 2 had enough episodes, although the alias is the second cour of season 1. The alias must be mapped to the season, or IMDb must end the previous season exactly where the cour begins.
- **Numbered parts.** `<first part> Part N - EE` resolves the N-th AniDB work of the season. The episode is local to the part or continues the season's numbering (`Part 2 - 25` when the part is episodes 13 to 25).
- **Official numbering after a prologue.** When Anime-Lists maps exactly AniDB episode 1 to a special and the remaining episodes with offset -1, and IMDb's season has exactly the remaining count, a bare number N is official episode N (AniDB N+1). Two AniDB works in the index have this shape.
- **A numbered title is not an episode.** When the complete title with its number is an exact anime alias or an exact IMDb series title (`Mob Psycho 100`, `One-Punch Man 3`), the trailing number is no longer offered as an episode.
- A title that keeps only digits after folding cannot verify an episode candidate.
- **A running number continues into the next season.** The baseline accepts this only when the next season's AniDB work has a bounded map. It now also holds when that work is the only directly linked work mapped to the season and starts it at episode 1, and when an open-ended later part had claimed the number with an episode beyond IMDb's bound. The release's work is taken from its exact title alias; the IMDb record's single stored AniDB ID is only the fallback.
- **IMDb splits a TVDB season at the cour.** When the crosswalk places an episode beyond IMDb's bound and that season ends exactly where the cour begins, the episode is the cour-local episode of the next IMDb season with the same ASID.
- **A nominal `S01` marker on an absolute number.** `S01E147` is accepted when the absolute crosswalk maps exactly that AniDB episode to a later season that IMDb knows to contain it.
- **Batches of a numbered part** resolve both endpoints through the part and become a season-relative range.
- **ASID for a final coordinate.** A selected single episode gets its ASID from the season crosswalk whichever lane selected it, including a later cour that IMDb keeps inside the TVDB season. For a coordinate inferred from IMDb season lengths the crosswalk must agree on where the season begins; otherwise the SID is published without an ASID (the baseline attached AniDB episode 109 of the first Gintama series to IMDb season 3 episode 10).

The matcher semantic revision is now `anidb-parts-and-numbered-titles-v7`.

## Verification and limits

- Workspace Rust tests: **575 passed**. Python discovery: **99 passed**. All-targets check with warnings denied.
- `anidb_parts_prologues_and_numbered_titles_agree_with_parsed_episodes` fails on the baseline and passes with the change.
- Local replay measures agreement between two builds on one metadata snapshot. It does not establish that an unchanged result is correct.

Two baseline coordinates are withdrawn: a batch of a sequel that the baseline assigned to the earlier series of the same name, and a bare numbered title that the baseline read as an episode of a shorter-named anime. They now return the series without coordinates.

Not addressed here:

- Series where IMDb and TVDB divide the seasons differently and no cour boundary explains it (140 blocked rows). The SID can be inferred from IMDb, but no ASID is published.
- A range of a numbered part gets its SID endpoints but no ASID endpoints.
- 540 rows still have a SID episode without an ASID, mostly season packs and works without a season crosswalk.
- Identity ambiguity between a remake and its original when both carry the same alias and the release has no year.

Raw production filenames, publisher labels, debug records and credentials remain outside Git. See the [sanitized measurements](measurements/anidb_discrepancy_2026-11-15.json).
