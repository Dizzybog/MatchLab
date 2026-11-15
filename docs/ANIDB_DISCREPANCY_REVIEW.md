# AniDB parser and matcher discrepancy review

Date: 2026-11-15. This review follows [the third protected-feed review](PRODUCTION_DEBUG_ROUND_THREE.md) and starts from merged PR #153, commit `a8d8f66`.

## Evidence

Two bounded reads of the protected debug feed hold **136,215 history records (141,661 unique request strings)**. **12,723 unique request strings** were matched with AniDB evidence (an ASID, a linked AniDB ID, the anime-priority lane or an anime candidate). The feed is a rolling window, so this is a sample, not a snapshot.

Unlike the earlier reviews, the matcher was replayed locally. The seven public metadata sources were imported with the baseline binary, and every AniDB-related request string was sent to two local servers that share this index: the baseline `a8d8f66` and this change. Both ran with `MATCHLAB_STRICT_MODE=false`, as production does. Recorded production outputs come from older builds and were used only to select the request strings.

A row counts as a discrepancy when the parser and the matcher disagree: a parsed episode or range without a selected coordinate, a season that differs, a SID without the ASID of a linked work, or a title that still ends in an unparsed number. These labels are heuristics, not correctness labels.

## Result

The change alters **293 of the 12,723** AniDB-related results. 261 gain an episode, 10 gain a range end, 10 change season or episode, 6 change IMDb identity and 3 lose a coordinate that the baseline assigned to the wrong series or season.

| Baseline | This change | Discrepancy class |
| ---: | ---: | --- |
| 10,956 | 11,204 | Consistent |
| 276 | 82 | Title ends with a number, no episode candidate |
| 259 | 196 | Range parsed, no range end selected |
| 185 | 131 | Episode parsed, no SID episode (not blocked) |
| 577 | 580 | SID episode without an ASID |
| 362 | 364 | Episode parsed, SID episode blocked by known bounds |
| 223 | 222 | Unmatched |

A random sample of 20,000 request strings without AniDB evidence changes in 21 rows. Every changed row in both sets was read; the three lost coordinates are listed under limits.

## Parser changes

| Defect | Correction |
| --- | --- |
| A fansub episode written without a dash before the first tag produced no coordinate (`[Group] Title 12 [CRC].mkv`, `Title_07_(HD.720p)`, `Title!-05.mp4`, `Title 04 VOSTFR`) | It is published as an unverified anime episode candidate. Two to four digits are required, a following year excludes it, and dotted scene names, dated releases and product codes are not candidates |
| A zero-padded batch followed by a subtitle or a DVD/TV tag was expanded into an episode list, of which only the first episode was matched (`Title - 0575-0746 - Arc name`, `Title 1-101 dvdrip`) | It is an unverified range candidate |
| The range candidate's title was parsed from the prefix alone and could lose its tail (`Yu-Gi-Oh!` became `Yu-Gi`) | The complete release title is kept when it only extends the prefix title |

Replaying all 141,661 request strings through both parsers changes 438 results: 406 gain an episode candidate, 33 change a range candidate and 23 change `episodes`. Four files inside a batch folder now carry their own episode instead of the folder's range.

## Matcher changes

- **An exact alias is not a shortened name of an unrelated prefix.** The shortened-AKA penalty demoted `Dragon Ball Kai`, whose primary IMDb name is longer, below the prefix `Dragon Ball`. The penalty now applies only when another candidate carries the release title as its primary name.
- **Part-titled episodes and batches.** When several AniDB works of one IMDb series accept the same absolute number, an unmarked episode or batch resolves through the exact part alias, as the dashed form already did. A bounded AniDB work that independently proves the episode keeps priority.
- **A numbered alias proves a season only when it names that season.** `Title Part 2 - 05` was promoted to IMDb season 2 because season 2 had enough episodes, although the alias is the second cour of season 1. The alias must be mapped to the season, or IMDb must end the previous season exactly where the cour begins.
- **Numbered parts.** `<first part> Part N - EE` resolves the N-th AniDB work of the season. The episode is local to the part or continues the season's numbering (`Part 2 - 25` when the part is episodes 13 to 25).
- **Official numbering after a prologue.** When Anime-Lists maps exactly AniDB episode 1 to a special and the remaining episodes with offset -1, and IMDb's season has exactly the remaining count, a bare number N is official episode N (AniDB N+1). Two AniDB works in the index have this shape.
- **A numbered title is not an episode.** When the complete title with its number is an exact anime alias or an exact IMDb series title (`Mob Psycho 100`, `One-Punch Man 3`), the trailing number is no longer offered as an episode.
- A title that keeps only digits after folding cannot verify an episode candidate.

The matcher semantic revision is now `anidb-parts-and-numbered-titles-v7`.

## Verification and limits

- Workspace Rust tests: **573 passed**. Python discovery: **99 passed**. All-targets check with warnings denied.
- `anidb_parts_prologues_and_numbered_titles_agree_with_parsed_episodes` fails on the baseline and passes with the change.
- Local replay measures agreement between two builds on one metadata snapshot. It does not establish that an unchanged result is correct.

Three baseline coordinates are withdrawn: a `Part 2` batch that the baseline placed in IMDb season 2 although the part belongs to season 1, a batch of a sequel that the baseline assigned to the earlier series of the same name, and a bare numbered title that the baseline read as an episode of a shorter-named anime. They now return the series without coordinates.

Not addressed here:

- A running episode number that crosses into a later season whose AniDB work restarts at 1 (`Title - 21` for season 2 episode 9). The baseline deliberately requires the target ASID to equal the absolute number.
- Batches of a numbered part (`Part 2 - 01 ~ 12`).
- A linked AniDB work without an ASID for an explicit `SxxEyy` in a later cour (580 rows).
- Identity ambiguity between a remake and its original when both carry the same alias.

Raw production filenames, publisher labels, debug records and credentials remain outside Git. See the [sanitized measurements](measurements/anidb_discrepancy_2026-11-15.json).
