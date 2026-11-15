# Machine Learning overview

This is a summary of the optional local learner. For the full design, see [Machine Learning and Community Learning](COMMUNITY_LEARNING.md), [ML regex engine](ML_REGEX_ENGINE.md), [ML scaling](ML_SCALING.md) and [ML performance](ML_PERFORMANCE.md). To share learned rules on GitHub, see [Community sharing setup](COMMUNITY_SHARING_SETUP.md).

## How it works

**MatchLab works normally without Machine Learning (ML).** When enabled, a small Rust model learns patterns from release names in the background. It can add new regex patterns or suggest replacements for existing ones. **ML never sets or overwrites parsed fields.** MatchLab's own parser interprets the compiled rules, and its matcher verifies IMDb/AniDB identities as usual. The model is not run on each request.

The learning process is:

1. Examine new release names for **four error types**: missing values, excessive extraction, incomplete extraction, and values assigned to the wrong fields (for example, swapped seasons and episodes). These are learning signals, not automatic corrections.
2. Group recurring patterns and train a small **FTRL-Proximal** model. When a trustworthy IMDb winner is available, compare its matched title alias to the **original canonical parser title** and the complete source-title words before an explicit episode, year or quality boundary. Confirmed over-/underextraction becomes another title-learning signal; aliases that cannot be verified in the raw release are not used as labels.
3. Test proposed regex rules against known values, conflicting patterns and negative examples.
4. Activate eligible rules in memory without restarting. More uncertain changes are kept for review.

The generated parser contains **465 ordered handlers across 48 field families**
(456 regex handlers and nine Rust functions). The new *field-general*
learner can derive safe regex candidates from existing native handler
provenance and meaning-preserving release-tag variants. It must prove
**incremental improvement**, preserve complete parser results, reject
duplicate/no-op rules and revalidate stored evidence after upgrades.
More ambiguous changes remain review-only until independent references
establish a reliable result. See the [complete parser and ML regex audit](ML_REGEX_ENGINE.md)
for field families, transform safety, title-removal interactions, rule
lifecycle and remaining limitations.

**ML learns from every field that MatchLab parses**, including titles, years, seasons, episodes, languages, video and audio details, release groups, and other metadata. It looks for recurring naming patterns and can propose **new regex rules or improvements to existing ones** for any field. A rule is activated in the live parser **without restarting** only when sufficient independent evidence and field-specific safety checks support it; otherwise it remains inactive for further validation or review. Anime episode patterns still require verified metadata mappings. **ML changes regex only: MatchLab performs all field extraction and IMDb/AniDB matching. The existing `date` field participates in the native ML handler catalog, so new date syntaxes can be observed and proposed; unlike fixed technical labels, date changes require calendar and full-parser verification before they can become live rules.** With community sharing enabled, the exact regex additions and replacements can be proposed on GitHub. An ML score alone never activates a rule.


**Title discrepancy learning:** A title mismatch is not automatically a parser error:
IMDb may store translated aliases, different anime-part names, or competing
works. MatchLab trains on title-boundary discrepancies only when the
selected alias exactly covers the independently extracted source-title span
from a strict public match. Repeated observations do not increase evidence
counts, and at least 12
examples from four IMDb works before suggesting a bounded, generic
title-boundary regex. Such proposals are **review-only** and never change the
live parser until a trusted source diff and regression tests separately prove
that the new rule improves results. No torrent filenames, matched movie
names or IMDb IDs are exported in these proposals. Local status is available
under `/api/ml/status` as `title_boundaries`.


Enable learning with **one ML environment variable**:

```yaml
environment:
  MATCHLAB_ML_SIZE_GB: "10"
```

`0` (the default) disables local ML; `1` to `128` enables it with that
many GiB reserved as a **best-effort total ML budget**, not an operating-system
filesystem quota. MatchLab splits this into **50% training evidence, 30% result
cache and 20% observation queue/history**. A 10 GiB budget derives up to
500,000 training examples, 500,000 cached responses and 900,000 queued
observations. Actual disk usage is governed independently by occupied SQLite
pages and estimated queue bytes. Safety/regex activation thresholds and CPU
concurrency do not grow with the budget.

See [ML capacity and scaling](ML_SCALING.md) for size profiles, migration
from the older ML variables and what is excluded from the budget.
For background training CPU efficiency, native parser replay caching,
SQLite evidence revisions, FTRL feature reuse and the division of work
between Tantivy and SQLite, see [ML runtime performance](ML_PERFORMANCE.md).

**Safe observation mode:** set `MATCHLAB_ML_LIVE_REGEX_ENABLED: "false"` to keep
collecting/training locally while preventing learned regex rules from changing
the live parser. This is an emergency opt-out for native regex publication,
not a switch to disable the canonical parser. Re-enable only after examining
`GET /api/ml/status` and the local regression report.

Only `MATCHLAB_ML_SIZE_GB` is needed for local ML. The training and queue
limits are estimated numbers of retained observations, not RAM reservations;
training batches are chosen automatically. The separate bounded SQLite ML job
queue stores observations after matching. Cache hits use a small in-memory
tier: HTTP parsing/matching never opens the cache SQLite file. Cache writes run
in their own background worker, separately from training. After parser, rule
or metadata changes, old cached answers are ignored and requests are parsed/
matched again. Learning evidence is never treated as a completed match.

Check `GET /api/ml/status` to see local model and rule statistics. No separate AI server, GPU or online AI service is required.

## ML novelty gate (parser improvement, not regex duplication)

The model still trains from every parser field, including successful canonical parses, but a technical marker is only **activated as a new regex** after at least **four independent missing-field observations covering two title identities**, alongside the existing minimum of eight examples and four distinct title identities. A valid native parser handler must exist for the exact field/transformation; otherwise the candidate remains training or review evidence. This prevents standard markers like `x264`, `1080p`, `mkv` and `subbed` from inflating the active-rule count merely because they are common.

TV families require three distinct missing-coordinate observations, two distinct affected titles and two correct original-parser confirmations before activation. Conflicting coordinates never count as missing. Learned regexes still use MatchLab's original field transforms, never directly produce field values, and are shared only after local validation. Historical rules remain archived; an updated build may initially show fewer **active** rules without deleting its learned evidence.

`GET /api/ml/status` now reports `evidence_only_candidates` and an `activation_policy`. This intentionally prioritizes useful changes over the raw number of generated patterns.
