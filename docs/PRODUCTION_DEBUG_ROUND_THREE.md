# Third authenticated parser and matcher review

Date: 2026-11-15. This review follows [the second protected-feed review](PRODUCTION_DEBUG_ROUND_TWO.md) and starts from merged PR #148, commit `f25e0df`.

## Evidence

A bounded read of the protected debug feed collected **60,235 history records (59,255 unique request strings)** between `08:07Z` and `08:40Z`. The feed is a rolling window, so this is a non-exhaustive sample, not a snapshot. Every recorded request used `source_policy=public_api`, `strict_mode=false` and semantic revision `complete-title-precedence-and-metadata-boundaries-v5`. Five cohorts of 1,000 unique request strings each were drawn from it:

| Cohort | Definition |
| --- | --- |
| A | Parser quality is BluRay/BDRip/BRRip/REMUX/DVD/DVDRip and the matcher selected no IMDb ID |
| B | Matched, and the selected IMDb year differs from the parsed year by more than one |
| C | Matched, unique request string and unique IMDb ID |
| D | Matched, and the parsed season/episode differs from the selected season/episode |
| E | Matched, and the parsed title word count differs from the selected title's |

Cohort labels are heuristics. They are leads, not correctness labels, and none of the fractions below is an accuracy estimate.

## Findings

### A: disc releases without an IMDb ID

| Rows | Pattern |
| ---: | --- |
| 655 | Latin title with no alias in the index (transliterated titles, adult and non-catalogue content, typos, direct-to-video titles) |
| 247 | Cyrillic or other non-Latin title with no alias |
| 32 | Exact alias with a matching year, but a bogus season/episode made the film a series request. The parser changes below clear 18 |
| 27 | Exact alias with a matching year, but the work is a series and a year without coordinates admitted only films |
| 22 | Exact alias, year more than one away |
| 16 | Exact alias, release has no year |

### B: year more than one away

| Rows | Pattern |
| ---: | --- |
| 336 | Film selected through a shorter or different title |
| 237 | Film with the same title from another year (186 more than two years away) |
| 222 | Series with season 2 or later where the release year is that season's year. These are plausible matches |
| 100 | Series with the same title, season 1 or no season. Mostly a same-name anime with a crosswalk replacing the ranked winner that agreed with the year |
| 65 | Series selected through a shorter or different title |
| 40 | Non-Latin title whose compact key kept only digits |

### C, D and E

- **C:** 934 of 1,000 are an exact title with an agreeing or absent year. 20 exact-title rows are more than one year away and 29 select a shorter title.
- **D:** 812 rows are a season pack gaining episode 1 or a bare anime episode gaining its season; 72 are date-based episodes; 62 are bare anime titles expanded to a series range. These are intended. 34 rows drop coordinates that exceed known bounds.
- **E:** 658 select a title shorter than the parsed one, 120 a longer one (mostly country-qualified series), 122 differ only in word splitting and 98 are translated aliases.

## Parser changes

| Defect | Correction |
| --- | --- |
| A year glued to the title removed the last title letter (`Title(1989)` lost its final letter, and a trailing `Ru`/`Rus` remainder became a language) | The year handler consumes the preceding character only when it is not a letter or digit |
| Release-group digits became a season (`-…t0`, `-…t99`, `…T69`) through the French `T01` handler | The marker requires a non-alphanumeric left boundary |
| A season pack took an episode from a later tag (`S02 … 3Audio`, `S01 … 1080p`) once the tags between them were removed | The dot-only season/episode separator must be directly adjacent (`S01.05`) |
| `QHD` inside a site name became 1440p, which hid the real resolution | `QHD`/`WQHD`/`QuadHD` require letter boundaries |
| An enclosed one-episode batch `(24-24)` stayed in the title and produced no coordinate | It is published as an unverified one-episode anime candidate and removed from the title. Bare `13 - 13` forms are unchanged |
| `Sample/sample.mkv` became the title `sample` | `sample`/`samples` are generic path names, so the leaf inherits its release folder |

Replaying all 59,255 captured request strings through the baseline and corrected parsers changes **244 results**: title (119), path title candidates (66), seasons (47), episodes (38), site (12), resolution (10), languages (7) and one new range candidate. Every changed row was inspected. Six season packs tagged `3xRus` lose the `ru` language that the old episode match exposed by accident; no other regression was found. Unchanged rows are not assumed correct.

## Matcher changes

- **A production year can identify a series.** A release with a year and no coordinates was always a film request, so `Trigun (1998)` selected the 2010 film alias. It is now a series request when a complete series title (IMDb alias, or an exact AniDB alias of one IMDb series) has exactly that start year and no film with the same complete title was produced within one year.
- **The year-agreeing winner keeps its place.** With a known year, the first candidate with an anime crosswalk replaced the ranked winner (`Peacemaker (2022) S01E04` selected the 2003 anime). A candidate more than one year away no longer replaces a winner that is within one year and whose requested episode IMDb knows.
- **Films from another period are not fallbacks.** In broad mode a film request with a year keeps candidates within one year, or within two when it is the only complete-title identity. Strict mode and series requests are unchanged.
- **Number-only key collisions are rejected.** When folding leaves only digits of a title that has letters, an alias must have the same Unicode words.

The matcher semantic revision is now `year-identity-and-release-group-boundaries-v6`, so persisted misses from the previous rules are not reused.

## Verification and limits

- Workspace Rust tests: **555 passed**. Python discovery: **99 passed**. All-targets check with warnings denied.
- `release_year_selects_identity_over_same_name_films_and_anime` fails on the baseline (it selects the film) and passes with the change. Two existing matcher tests asserted that broad mode keeps a film from another decade; they now assert the new rule.
- There is no local metadata index, so the captured cohorts were **not replayed through the corrected matcher**. Matcher behaviour is covered by isolated fixtures only, and the effect on the production cohorts is unmeasured.

Not addressed here: IMDb `video` titles are not imported; transliterated and non-Latin titles without an imported alias; aspect ratios and file sizes read as coordinates (`[16x9]`, `[1.46]`); codec numbers read as episodes; and a Latin key such as `com` colliding with a non-Latin alias.

Raw production filenames, publisher labels, debug records and credentials remain outside Git. See the [sanitized measurements](measurements/production_debug_round_three_2026-11-15.json).
