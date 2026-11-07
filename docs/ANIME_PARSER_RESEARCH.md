# Anime release parsing: Rakun and AIOStreams review

This document records a source-level review of:

- [lowlighter/rakun](https://github.com/lowlighter/rakun): its parser, regex families, staged processing and release fixtures.
- [AIOStreams/release-parser](https://github.com/AIOStreams/release-parser): the entire parser pipeline, handler table, transforms, validators, title cleaner, episode-title extraction, prefilter and test fixtures. This is the release-name parsing library actually used by [Viren070/AIOStreams](https://github.com/Viren070/AIOStreams), not AIOStreams' streaming/formatter regex table alone.
- [AIOStreams application file parsing](https://github.com/Viren070/AIOStreams/tree/main/packages/core/src/parser): torrent/file/folder parsing, merge behavior, name caching and media tag extraction.
- MatchLab's own Rust handler engine, 458 generated handlers, existing anime matcher and file-path merge.

These repositories are reference implementations and test-corpus sources, not runtime dependencies. The change in this PR is independently implemented in Rust and intentionally **does not copy** the TypeScript parsers or add JavaScript/Python components.

## Findings, coverage and choices

| Pattern / technique | Reference | MatchLab status / action |
| --- | --- | --- |
| Treat fansub prefix `[Group]` and trailing `[8-hex CRC]` as release metadata, not show titles or episode numbers | Rakun, AIOStreams release-parser | Existing handler table supports group / episode-code tags; added a conservative `#05` episode recognizer guarded by actual fansub/release context |
| Extract specific numbered episodes before treating free numeric words as episodes; protect digits in titles, years, dimensions, codec names and film numbers | Both parsers, especially AIOStreams episode validators | Existing explicit marker handlers and false-positive guards retained; extra cases added for hash and range syntax |
| `Eps 01-12`, `E01-E12`, `Episodio 93~94`, `S01E01-S01E12`, `01-12 + OVA`, `v2` corrections | Rakun series regexes and AIOStreams episode handlers/tests | Existing Sxx ranges and tentative unmarked anime ranges retained; added labelled multilingual/tilde ranges with endpoints rather than expanding large arrays |
| Preserve `Season`, `Part`, `Cour`, `Movie`, `Volume`, `Chapter`, and anime arc names distinctly; do not turn film numbers into episode IDs | Rakun movie/part/season split and AIOStreams season validators | Retain existing safeguards; reject film/volume/part prefixes for new `#` syntax |
| Bracketed Japanese/Chinese/Korean titles and explicit `第NN集`, `第NN話`, `제NN화` markers | Rakun Asian-bracket normalization, multilingual release examples | Extend existing Chinese absolute-episode *hint* for traditional/simplified Japanese/Korean markers. A hint never by itself changes a verified season/episode |
| Audio/subtitle languages differ, and `MultiSub` is not automatically `MultiAudio` | Rakun separate subtitle/audio regexes; AIOStreams language handlers | Existing language/codec parser and translation table remain; no unsafe guess of extra languages |
| Extract the clean title before technical tags, but never consume a semantic title word merely because it is `WEB`, `HD`, `Part`, etc. | AIOStreams title extraction, `retryPastTitle`, validators | Existing metadata guards retained; no wholesale replacement of 458 generated handlers |
| File and parent-folder metadata should be parsed independently; the more specific file wins for coordinates, with explicit title inheritance only when needed | AIOStreams file+folder merge and MatchLab path parser | Existing MatchLab hierarchical parse retained; fix: stale parent absolute-episode hints must not survive a *different* explicit child episode |
| Multiple video files are not necessarily a single season or even a single IMDb title | AIOStreams file-level parsing, Rakun batches | Already handled downstream by StremThru/MatchLab per-file matching and verified AniDB crosswalks, not inferred from a single filename |
| `seasonPack` from folder/file size ratios and >5 episode numbers | AIOStreams `applySeasonPackHeuristics` | Not copied: `/parse` takes a single name without sizes; guessing from a synthetic filename would be incorrect |
| Cache repeated release-name parses; avoid recompiling regexes and running unneeded handlers | AIOStreams bounded LRU, AIOStreams release-parser prefilter | MatchLab already holds compiled PCRE2 handlers and uses an Aho–Corasick prefilter, with separate server-side parse caching |
| Episode-title extraction after a structured episode token (without swallowing codec, language or subtitle tags) | AIOStreams `extractEpisodeTitle` | Not added to this PR: identity matching consumes title and numeric coordinates, not episode descriptions; copying the large stopword/segment processor risks false aliases |
| Validate identity and map absolute episodes only with authoritative anime data | MatchLab AniDB/TVDB resolver | Never fabricate an AniDB ID, a season offset or an episode end from an unmarked title range |

## Parsing precedence

1. Preserve the original release string as input. Extract canonical handler metadata without destructive pre-cleaning of names.
2. Apply **explicit** coordinate syntax first (`SxxEyy`, `E768-E1064`).
3. Accept labelled single-episode / range syntax only in recognized release context, protecting film parts, title numbers and CRC checksums.
4. Keep **unmarked** anime `01-12` ranges as unverified candidates until an exact AniDB-linked title is found.
5. Let each file override older conflicting parent range information. An inherited `第41集` absolute marker is usable only when the deeper file actually says episode 41.
6. Use IMDb/AniDB metadata and season crosswalks in the server. Parse output does not independently assert that an absolute episode is a specific season episode.

## Regression coverage

`crates/matchlab-parser/tests/anime_release_conventions.rs` exercises:

- Rakun-style `#05`, `#09v2`, checksum-tag releases;
- labelled Spanish, English, tilde, ampersand and abbreviated ranges;
- season-relative ranges and existing exact `SxxEyy-SxxEzz` versus absolute `E768-E1064` ranges;
- CJK and Korean explicit episode markers (as candidate hints);
- parent/leaf absolute-number agreement and disagreement;
- numeric anime titles, movie numbers and volume range false positives.

Existing `anime_episode_ranges.rs`, `chinese_release_path.rs`, `history_regressions.rs`, server crosswalk tests, and handler-prefilter equivalence tests must continue to pass.

## API and performance

`POST /parse` remains the existing JSON object; the added range metadata uses the already supported `episode_start`, `episode_end`, `episode_count`, and `episode_range_numbering` keys. The absolute marker continues to use `absolute_episode_candidate` and requires crosswalk verification.

The public `POST /match` four-field SID/ASID contract is unchanged. This PR introduces no external runtime dependency, database migration, new metadata downloads or broad handler-table regeneration.
