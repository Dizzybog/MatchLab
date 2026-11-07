# Release-name parser audit: AIOStreams and Rakun

This note records a field-by-field review of
[AIOStreams/release-parser](https://github.com/AIOStreams/release-parser)
(MIT), its integration in [AIOStreams](https://github.com/Viren070/AIOStreams)
(AGPL-3.0), and [Rakun](https://github.com/lowlighter/rakun) (MIT),
against MatchLab's native Rust parser and metadata-backed IMDb/AniDB matcher.

The objective is **not** to import an alternate parser wholesale.
MatchLab already has a native handler table and deliberately conservative
episode coordinates. An unverified number in a filename must never become
an invented AniDB/TVDB episode identity.

## Scope and architecture

| Area | AIOStreams | Rakun | MatchLab decision |
| --- | --- | --- | --- |
| Handler engine | Ordered patterns with transforms, validators, `skipIfFirst`, `skipFromTitle`, `keepMatching`, `retryPastTitle`, and `mustEndTitle` | Ordered extraction followed by cleaners | Preserve the native Rust engine and handler priorities |
| Fast regex filtering | Bitset/gram prefilter, incremental updates after removing tokens | Simple regex sequence | Preserve MatchLab's Aho-Corasick/PCRE2 prefilter; parity tests must not sacrifice speed |
| Movie/series title | Prevent year/quality tokens from cutting a real title, distinguish bracketed groups and alternate scripts | End-first cleanup of trailing square-bracket tags; retain all-bracket titles | Use existing title cleaning and mixed-script normalization |
| Year/date | Multiple calendars, ranges, ordinal dates, country-specific naming | Minimal | Keep existing year/date handlers and safeguards for numeric titles |
| Seasons | `Sxx`, `x`, season words in several languages, lists/ranges, and cross-language names | Season/Part/Movie markers, including ordinal season language | Existing MatchLab handlers plus conservative coordinate corrections; season pack is not an episode pack |
| Episodes | Explicit S/E, `x`, absolute and zero-padded, multi-episode, `v2`, `X of Y`, and ambiguous numeric tests | `#05`, `13 END`, E/Ep, `Episodio`, ranges with `-`, `~` and `&` | Add only strongly delimited fansub fallback; continue guarding years, resolutions, volumes, parts and titles |
| Episode title | Extract after anchored episode marker; stop at metadata, language, group, adjacent markers and bracket boundaries | Cleans residual title after removing episode markers | Add optional `episode_title` field, not an authoritative episode mapping |
| Release group/hash | Guard `Spider-Man`, film-title hyphens, suffix tags and `[Group]` | Bracketed fansubs and eight-character checksums | Keep group/parser behavior independent of IMDb matching; do not interpret checksums as episodes |
| Video | Quality/source, resolution, codecs, HDR/DV, bit depth and `3D` | Pixel dimensions, `Hi10P`, `YUV444P10`, codecs | Existing handler table covers most values; do not replace 432 handlers with a second pass |
| Audio | Channels, multitrack audio, multiple languages, dubbed/subbed | Dual/multi audio, DTS-HD MA, `EAC3` and aliases | Keep existing multi-value audio/language outputs and deduplication |
| Languages | Broad script/token support and special-case collisions; AIOStreams wrapper merges decoded language tags | Japanese/English/French/Spanish/Italian and subtitle markers | Distinguish language tags from actual titles (e.g. `English as a Second Language`) |
| Release metadata | Complete/collection, edition, remaster, repack, subtitles, language, network, extension and accessibility variants | OVA/special, Batch, Complete, Part, RAW, TV networks, version | Favor explicit metadata; do not guess missing episodes from `Complete` or `Batch` |
| Folder/file names | AIOStreams parses leaf and folder, selects richer metadata, and applies size-based season-pack heuristics | Primarily filenames | MatchLab already parses each path component; deeper coordinates override ancestors without leaking stale range data |
| Cache | Bounded 10k-title LRU and configurable repost suffix stripping in AIOStreams application wrapper | None | Avoid a second parser cache; MatchLab already has an API/query cache and native prefilter |

The AIOStreams application depends on **`@aiostreams/release-parser`** for the
actual release parsing. Its `packages/core/src/parser/` files are the
integration and metadata-merging layer, not a separate anime-only parser.
Its folder-size heuristic (`folderSize > 2 * individualFileSize`) cannot
run safely in MatchLab's standalone title-only `/parse` API, which has no
trusted file-size data.

## Implemented in this PR

1. **Optional episode title:** A structured explicit S/E, x-style, or Ep/Episode
   marker can produce `episode_title`. The series name and existing
   `seasons`/`episodes` remain untouched. This is descriptive metadata,
   not an IMDb/AniDB identity.
2. **Release metadata guards:** Stop at standard resolution, codec,
   quality, audio, subtitle, group, release type and language markers.
   Do not infer an episode title from a bare anime episode number.
3. **Rakun-style anime cases:** Bracketed fansub group plus `#05`, and
   delimited `- 13 END`, are conservative episode fallbacks only when
   the existing parser has no coordinates.
4. **Folder isolation:** A child with a new episode/season does not inherit
   `episode_title` from a different parent release.
5. **Regression tests:** Named episodes, films, tech tags, multiple anime
   release conventions, and negative cases including metadata mistaken for
   an episode title.

## Important non-goals

- Do **not** make every bare `01-12` a canonical episode range. MatchLab
  exposes ambiguous batches as *candidates* and verifies actual anime IDs
  against its local AniDB/TVDB crosswalk before writing IDs.
- Do **not** infer absolute-to-season episode numbers from filename-only data.
- Do **not** guess OVA or specials identifiers from `Complete`, `Special`
  or `Batch` tags.
- Do **not** blindly normalize every resolution or overwrite an existing
  trusted file identity with a weaker folder-level interpretation.
- Do **not** import code from the AGPL-licensed AIOStreams application.
  The implementation uses original Rust code informed by documented behavior
  and test ideas from the separate MIT-licensed parsing projects.

## Follow-up opportunities (not part of this patch)

Further release-parser parity should be implemented only with dedicated
negative tests and performance measurements: richer subtitle-language variants,
accessibility editions (`ASL` and audio description), release-group extraction
near parentheses, language/website TLD disambiguation and edge cases involving
numbered movie titles and foreign-language season markers.

The existing parser is already substantially feature-rich. A comparison of
field names or handler counts alone is **not** proof of semantic coverage:
the 439 AIOStreams handlers and 432 MatchLab handlers are different schemas
and contain overlapping rules.
