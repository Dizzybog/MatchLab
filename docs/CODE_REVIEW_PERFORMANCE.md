# Code review: dead code, duplicate work and request cost

Date: 2026-11-16. Baseline: `main` at `dba6992` (PR #154).

## Method

- **Execution coverage of real traffic.** An instrumented server answered 32,723 recorded request strings (every AniDB-related one plus a random sample of 20,000 others) against the full public metadata, and the instrumented parser parsed all 141,661 captured request strings.
- **Execution coverage of the test suite** (575 Rust tests), merged with the traffic coverage. Code that neither executes is the candidate list for dead code.
- **Parser handler ablation.** Each of the 456 generated regex handlers was replaced in turn by a never-matching pattern, and 12,000 sampled request strings were reparsed.
- **Instruction profiles.** 400 sampled requests under callgrind, before and after.
- **Equivalence.** The 32,593 undated request strings were replayed through `main` and the change on the same metadata; dated releases were excluded because their episode lookup calls an external service.

## Findings

### Request cost

The callgrind profile of `main` attributed the matching work as follows:

| Share | Cost | Cause |
| ---: | --- | --- |
| 28% | Anime alias lookups | Each prefix lookup consulted the term dictionary of every one of the index's 22 segments |
| 15% | `memset` | Tantivy's `TermSetQuery` allocates and clears a bit set over the whole segment for every lookup |
| 12% | SQLite schema parsing | Every request opened a new connection, and SQLite parses the complete schema on a connection's first statement |
| ~3% | Alias word normalization | The ranker normalized the same alias title up to a dozen times per request |

### Dead code

The core matching and parsing code has almost none. Every function that real traffic never entered belongs to another endpoint (`/parse`, `/alias`, dashboard, status), the opt-in learning worker, strict mode (the default for other deployments) or a CLI import command. The test suite covers them.

Six public parser functions were used nowhere: `clean_title_native`, `translate_langs_codes`, `languages_translation_table`, `parse_title_context`, and the wrappers `install_handler_patches` and `install_learned_handler_regex`.

### Nondeterministic output

When several aliases of one IMDb work ranked equally, the published title came from Tantivy's hash order. Two runs of the same `main` binary on the same input differed in the displayed title for 197 of 6,000 requests (never in SID or ASID).

## Changes

- **Connections are reused.** Each blocking thread keeps its last read-only metadata connection. It is reused only for the same published file (device, inode, size, modification time), so an atomic metadata refresh always gets a new connection.
- **Term unions instead of `TermSetQuery`.** The lookups use tens of exact terms; a union of their posting lists returns the same documents without the bit set.
- **One segment per index generation.** The bulk index build disables background merging and merges once into a single segment before the generation is published. Existing generations are unaffected until the next metadata refresh. Building the 8.8-million-document index took 177 seconds including the merge.
- **Alias words are computed once** per candidate row, and the mixed-script filter skips ASCII titles.
- **Deterministic alias choice.** Remaining ties fall to IMDb's alias ordering and then the text.
- **Removed** the six unused parser functions, and applied performance-related clippy fixes (`rfind` instead of scanning to the last element, and similar).

## Results

CPU seconds of the server process for the 32,593 undated request strings, two runs each:

| Build | Index | CPU | Change |
| --- | --- | ---: | ---: |
| `main` | 22 segments | 311 | |
| this change | 22 segments | 242 | −22% |
| this change | 1 segment | 194 | −38% |

Over the 400 profiled requests (22-segment index) the instruction count fell from 7.43 to 5.29 billion (−29%).

All 32,593 results have the same SID, ASID and episode range as `main`, and no difference other than the displayed alias title. The single-segment and 22-segment indexes give identical complete responses.

## Not changed

- **147 parser handlers changed no result in the 12,000-title ablation.** Their positions are stored identities: learned rules in SQLite (`source_index`) and community proposals on GitHub (`handler_index`) refer to them. Removing them would renumber every later handler and invalidate those records. This needs a stable handler identifier first.
- **`translate_languages`** is passed through 51 parser functions and is `true` only in a benchmark. It is part of the parser's public API and costs nothing at runtime.
- **Clippy style lints** (collapsible `if`, argument counts) were left alone to keep this diff about behaviour-neutral performance.
