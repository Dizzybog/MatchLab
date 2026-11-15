# Third authenticated parser and matcher review

Date: 2026-11-15. This review follows [the second protected-feed review](PRODUCTION_DEBUG_ROUND_TWO.md) and starts from merged PR #148, commit `f25e0df`.

## Evidence

A bounded read of the protected debug feed collected **60,235 history records (59,255 unique request strings)** between `08:07Z` and `08:40Z`. The feed is a rolling window, so this is a non-exhaustive sample, not a snapshot. Every recorded request used `source_policy=public_api`, `strict_mode=false` and semantic revision `complete-title-precedence-and-metadata-boundaries-v5`. Five categories were drawn from it. The first 1,000 unique request strings of each were reviewed by hand, and every row of each category was then classified:

| Cohort | Definition |
| --- | --- |
| A | Parser quality is BluRay/BDRip/BRRip/REMUX/DVD/DVDRip and the matcher selected no IMDb ID |
| B | Matched, and the selected IMDb year differs from the parsed year by more than one |
| C | Matched, unique request string and unique IMDb ID |
| D | Matched, and the parsed season/episode differs from the selected season/episode |
| E | Matched, and the parsed title word count differs from the selected title's |

Cohort labels are heuristics. They are leads, not correctness labels, and none of the fractions below is an accuracy estimate.

## Findings

The tables cover **every** unique request string of the capture that fits a category, not only the first 1,000.

### A: disc releases without an IMDb ID (1,001)

Classified on the first 1,000 with a catalogue alias lookup per parsed title.

| Rows | Pattern |
| ---: | --- |
| 655 | Latin title with no alias in the index (transliterated titles, adult and non-catalogue content, typos, direct-to-video titles) |
| 247 | Cyrillic or other non-Latin title with no alias |
| 32 | Exact alias with a matching year, but a bogus season/episode made the film a series request |
| 27 | Exact alias with a matching year, but the work is a series and a year without coordinates admitted only films |
| 22 | Exact alias, year more than one away |
| 16 | Exact alias, release has no year |

### B: year more than one away (1,517)

| Rows | Pattern |
| ---: | --- |
| 447 | Film selected through a shorter or different title |
| 359 | Film with the same title from another year (282 more than two years away) |
| 360 | Series with season 2 or later where the release year is that season's year. Plausible matches |
| 156 | Series with the same title, season 1 or no season. In 56 the ranked winner agreed with the year and was replaced by a same-name anime with a crosswalk |
| 100 | Series selected through a shorter or different title |
| 95 | Non-Latin title whose compact key kept only digits |

The film year rule below rejects the recorded selection in 772 of these rows.

### C: all matched request strings (48,468)

- 44,088 are an exact title with an agreeing or absent year.
- 933 exact titles and 584 other titles are more than one year away.
- 441 selections used an alias whose non-Latin words are not written in the release.

### D: parsed and selected coordinates differ (8,280)

6,966 are a season pack gaining episode 1 or a bare episode gaining its season, 488 are bare titles expanded to a series range, 250 are date-based episodes and 195 are remapped absolute episodes. These are intended. 347 drop coordinates beyond known bounds.

### E: title word count differs (3,136)

2,033 select a title shorter than the parsed one, 401 a longer one (mostly country-qualified series), 414 differ only in word splitting and 281 are translated aliases. 479 of the film selections are also more than one year away.

## Parser changes

| Defect | Correction |
| --- | --- |
| A year glued to the title removed the last title letter (`Title(1989)` lost its final letter, and a trailing `Ru`/`Rus` remainder became a language) | The year handler consumes the preceding character only when it is not a letter or digit |
| Release-group digits became a season (`-…t0`, `-…t99`, `…T69`) through the French `T01` handler | The marker requires a non-alphanumeric left boundary |
| A season pack took an episode from a later tag (`S02 … 3Audio`, `S01 … 1080p`) once the tags between them were removed | The dot-only season/episode separator must be directly adjacent (`S01.05`) |
| `QHD` inside a site name became 1440p, which hid the real resolution | `QHD`/`WQHD`/`QuadHD` require letter boundaries |
| An enclosed one-episode batch `(24-24)` stayed in the title and produced no coordinate | It is published as an unverified one-episode anime candidate and removed from the title. Bare `13 - 13` forms are unchanged |
| `Sample/sample.mkv` became the title `sample` | `sample`/`samples` are generic path names, so the leaf inherits its release folder |
| A second year became an episode (`(2012) [DVDRip 2012]`, `(2003 to 2017)`) | An unlabelled year-like episode next to a production year is dropped; `E2004` stays |
| A file size or picture format became coordinates (`[1.46]` → S1E46, `[16x9]` → S16E9) | Dropped for a dated release without an explicit marker; undated `[2.14]` and `16x09` stay |
| A bare codec number became an episode (`h.-264`, `x-265`, `DVDRip 264`) | The codec prefix may be separated by punctuation, and a bare 264/265 next to a year is dropped; `[264]` of an undated anime stays |

Replaying all 59,255 captured request strings through the baseline and corrected parsers changes **311 results**: title (119), episodes (105), seasons (66), path title candidates (66), site (12), resolution (10), languages (7) and three single-row fields. Every changed row was inspected. Six season packs tagged `3xRus` lose the `ru` language that the old episode match exposed by accident; no other regression was found. Unchanged rows are not assumed correct.

## Matcher changes

- **A production year can identify a series.** A release with a year and no coordinates was always a film request, so `Trigun (1998)` selected the 2010 film alias. It is now a series request when a complete series title (IMDb alias, or an exact AniDB alias of one IMDb series) has exactly that start year and no film with the same complete title was produced within one year.
- **The year-agreeing winner keeps its place.** With a known year, the first candidate with an anime crosswalk replaced the ranked winner (`Peacemaker (2022) S01E04` selected the 2003 anime). A candidate more than one year away no longer replaces a winner that is within one year and whose requested episode IMDb knows.
- **Films from another period are not fallbacks.** In broad mode a film request with a year keeps candidates within one year, or within two when it is the only complete-title identity. Strict mode and series requests are unchanged.
- **Number-only key collisions are rejected.** When folding leaves only digits of a title that has letters, an alias must have the same Unicode words.
- **Mixed-script aliases need their own words.** Folding also made a Latin fragment equal to an alias such as `拿命.COM`. In broad mode an alias is used only when its non-Latin words are written in the release.

The matcher semantic revision is now `year-identity-and-release-group-boundaries-v6`, so persisted misses from the previous rules are not reused.

## Verification and limits

- Workspace Rust tests: **556 passed**. Python discovery: **99 passed**. All-targets check with warnings denied.
- `release_year_selects_identity_over_same_name_films_and_anime` fails on the baseline (it selects the film) and passes with the change. Two existing matcher tests asserted that broad mode keeps a film from another decade; they now assert the new rule.
- There is no local metadata index, so the captured cohorts were **not replayed through the corrected matcher**. Matcher behaviour is covered by isolated fixtures only, and the effect on the production cohorts is unmeasured.

Not addressed here: IMDb `video` titles are not imported (this needs a re-import and its index size and false-positive cost are unmeasured); transliterated and non-Latin titles without an imported alias, which are most of category A; and release-group or encode tokens such as `-F7` or `S89` read as coordinates.

Raw production filenames, publisher labels, debug records and credentials remain outside Git. See the [sanitized measurements](measurements/production_debug_round_three_2026-11-15.json).
