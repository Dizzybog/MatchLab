# Matching rules

How MatchLab decides whether a parsed release matches an IMDb or AniDB identity. For request and response formats, see the [API reference](API.md).

## Strict and diagnostic endpoints

The public `/match` and `/debug/match` endpoints have identical strict matching requirements. The dashboard's diagnostic endpoint is intentionally more permissive.

## Title word boundaries

Title matching preserves word boundaries in addition to Tantivy's compact normalized key: `Supernatural` (1 word) differs from `Super-Natural` / `Super.Natural` (2 words). **Strict** requires the original word count for exact IMDb and AniDB aliases. **Non-strict** uses the count only as a tie-break preference for normalized-key collisions, not as an exclusion: erroneous parser titles with additional words retain validated prefix/suffix recovery. Word counts are computed from existing names at query time; no metadata reindex is needed.

## Production year

**Strict production-year rule:** If a torrent filename supplies a year (or the `/match` request explicitly supplies one), the selected IMDb identity **must have that exact year**. Candidates with another year or no valid year are excluded, including AniDB-linked IMDb identities and episode-driven promotions. A release without a production year retains the existing title/episode matching rules. Non-strict mode continues to rank approximate years instead of excluding them. Date-based episodes use their separate airdate logic rather than interpreting an episode airdate as a show's production year.

The year in this example identifies the 2005 series. `Supernatural` also names
an anime series; without a production year, exact anime-title priority can
select that other identity. Include the year when known to disambiguate titles.

## Movie collections

A parent folder such as `Harry Potter (2001-2012)/` is a range, not an individual release year. For `Harry Potter (2001-2012)/Harry Potter 1 (2001)`, strict matching uses the child's exact **2001**. For `Harry Potter (2001-2012)/Harry Potter and the Sorcerer's Stone`, no single year is inferred; an exact film title with an IMDb year **within 2001–2012** may satisfy the public match eligibility gate. An out-of-range or unmatched title does not gain an identity from the collection label.

## Blu-ray folder layouts

For `Harry Potter and the Sorcerer's Stone/STREAM/0000.m2ts` (or `.../BDMV/STREAM/00000.m2ts`), the technical directory and numeric stream filename cannot become IMDb identities or fake episodes. The matcher inherits the individual film's full title from its parent. In strict mode this verified disc structure permits a yearless **unambiguous exact IMDb movie title** to pass the public release-evidence gate. If the exact title belongs to multiple IMDb works and no release year is available, the result stays unmatched instead of guessing; a production year still requires exact equality. Ordinary yearless bare filenames retain their existing public eligibility rules.

## Named anime parts and local seasons

A title such as `Sword Art Online - Alicization S2E02` can use `S2` relative to the named anime part rather than the global IMDb series. MatchLab first requires an **exact, unique AniDB part alias**, proof that this part belongs to a linked family, and a valid AniDB/IMDb **episode crosswalk**. If Alicization begins at IMDb S03 and its second part maps to S04, `S2E02` becomes IMDb S04E02; `S3E02` keeps global S03 because the explicit number equals the part's verified starting season. `P2` similarly selects a verified part, but it **does not necessarily mean another IMDb season**: JoJo Stone Ocean's parts can share IMDb S05 with episode offsets. Ordinary `Enen no Shouboutai S2` remains S02. Unverifiable local markers never manufacture SID or ASID endpoints; incomplete ranges remain unresolved. `/parse` publishes unverified local hints, while only `/match` can validate and convert them.

## Strict exact-title matching (default)

MatchLab compares
complete IMDb and exact AniDB aliases in the same ranked candidate set.
An unambiguous exact AniDB title can identify a named anime season/part even
when a different IMDb work has an exact title match. Explicit country-qualified
titles (such as `Lego Masters AU` / `Lego Masters Australia`) are treated as
verified complete aliases, never as partial matches. Strict still excludes
prefix/suffix, inside-title and original-release recovery. Normal
season/episode checks apply. With `MATCHLAB_STRICT_MODE=false`, the existing
broader search (including eligible shorter, whole-word IMDb title aliases and
original-release suffix recovery) is available without a title-length ban.
To opt out of strict mode, change `MATCHLAB_STRICT_MODE` to `false` and
restart MatchLab.
With no setting, strict remains enabled. Parsing (`/parse`) is unchanged.
See [strict mode behavior](API.md#strict-exact-title-matching-default). If metadata is missing or conflicting, MatchLab avoids inventing a match. See [API documentation](API.md) for request options, full responses and other endpoints.
