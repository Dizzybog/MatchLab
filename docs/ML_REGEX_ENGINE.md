# ML regex engine: native handler architecture and safety limits

The objective is to **improve every field of the native MatchLab parser over time**,
not to have an LLM rewrite returned JSON or let a learned rule override IMDb
matching. Regex is the only executable output of the learner.

## Complete parser inventory (generated source)

The checked-in `crates/matchlab-parser/src/generated/handlers.json` contains:

| Mechanism | Count | Why it matters |
|---|---:|---|
| Total handlers | 458 | Exact ordered processing pipeline |
| Regex handlers | 449 | Eligible for native regex candidates |
| Function handlers | 9 | Not safely editable as regex; may be affected indirectly |
| Distinct output field families | 48 | All are visible to the field-general model |
| Handlers that remove matched text | 253 | Can modify downstream matching and the final title |
| Handlers that short-circuit on a present field | 286 | Low-priority additions cannot correct a wrong value |

The largest native regex families include languages (126), episodes (39),
network (36), quality (33), seasons (28), complete (19), resolution (15),
audio (14), extras (10), and edition (11). Counts above exclude each
family's function handlers and can change when the generated catalogue
is updated; the ML status endpoint derives counts from the current source.

### How MatchLab actually parses a release

1. Normalize inputs and calculate an Aho-Corasick prefilter.
2. Run native handlers **in order**, using compiled PCRE2 regex or native
   Rust functions. A handler can choose a capture group, apply a transform,
   skip an already-populated field, or append into a result array.
3. Handlers with `remove=true` **mutate the working title**, which is then
   rescanned. `skipFromTitle` and `skipIfFirst` can alter title boundaries
   even when the field's value looks correct.
4. Run postprocessing: numeric ranges, inferred episode metadata,
   language conversions, and other semantic constraints.
5. Extract the remaining movie/series title from the mutated input.

**An isolated successful regex test is therefore NOT proof of a safe
parser improvement.** Every activation compares the *complete parser
result* (including title, coordinate ranges, languages and metadata) using
the real ordered native engine.

### Native transforms and their promotion boundaries

| Transform family | Examples | ML-safe candidate |
|---|---|---|
| Fixed `value(...)` | codec, resolution, network, edition, quality | Native ADD for independently recognized markers; general high-priority CORRECT is disabled without a separate validator |
| Boolean | dubbed, subbed, scene, 3D, flags | ADD only when the complete context and an approved marker establish a safe value; unknown strings are review-only |
| Array `uniq_concat(value(...))` | language, audio, HDR, channels, extras | Dedicated independently validated marker learner may ADD; general self-supervised promotion is blocked for additive array transforms |
| `lowercase`, `uppercase`, `none` | extension, site, region, group | ADD only when the transformed capture is independently verified; never assume a changed spelling is equivalent |
| `integer`, `first_integer`, `array(integer)`, `range_func` | year, seasons, episodes, volumes | Must preserve numeric captures, limits, ordering and episode ranges |
| Dates and resolution-specific transformation | date, resolution | Candidate-only until full converted value is proved; unsafe fallbacks remain inactive |
| Native Rust function | adult, group, episode postprocessing, language inference | Observe downstream effects, but do **not** manufacture a regex to replace function logic |

The catalogue is *not* a list of field-specific ML exceptions. It is the
only permitted source for field names, capture transforms, flags and
removal behavior. No model may inject new transformations, arbitrary
functions, SQL code, or field values into the parser.

## Three independent learning paths

**Native field learner (PR #69):** Tracks missing technical tokens and
verified successes. It can add native-source regex handlers or broaden
selected old expressions when an independent marker proves the value.

**Corrective learner (PR #87):** Tracks four error classes: missing,
overextracted, underextracted, misassigned/incorrect. Exact TV coordinates
provide independently readable reference values. New source handlers are
replayed at higher priority to determine whether previously nonempty bad
values can be repaired, including complete episode ranges.

**General native-handler learner (PR #87):** Observes source provenance
and errors in **all 48 field families**. The canonical parser identifies
its original handler, immutable native field/transform/options, and raw
match. The offline worker generates conservative delimiter and alphanumeric
boundary variations. These are *hypotheses*, not independent truths.
**Automatic activation is narrower**: only an ADD to a native fixed-value
or boolean handler with `skipIfAlreadyFound=true`, in an approved metadata
field, and with a marker in a **static known-public release vocabulary**.
It must repair at least four examples from two distinct title identities,
preserve the original sources and the full negative/real-release regression
corpus exactly, and demonstrate **incremental improvement** over already
accepted ML rules. The rest remains observational/review-only; self-generated
variants never authorize rewriting nonempty fields.

Changing an already populated **season or episode** remains possible
through the *separate* independently parsed TV-coordinate validator.
There is deliberately **no automatic high-priority general correction**
for title, year, release group, quality, codecs, subtitles or other fields
without an independent oracle. The native parser compiler rejects
non-TV high-priority rules even if one is supplied by a modified local DB.

The FTRL-Proximal statistical model remains a small advisory model
trained on all canonical fields; it cannot authorize a regex by
confidence alone. Rule induction uses empirical error/repair evidence
and strictly bounded grammar synthesis rather than LLM freeform output.

### Where general induction deliberately abstains

- A punctuation-only or numeric-boundary variant does not establish that
  two *different words* mean the same thing. New synonyms and typos need
  independently verified reference labels before activation.
- IMDb titles, release groups and years have high semantic ambiguity.
  A parser success is not proof of a correct IMDb identity; matched
  metadata and independent references remain necessary for stronger fixes.
- Some regexes use PCRE2-specific lookaheads or capture conventions. A
  generated regex is restricted to linear-time Rust-regex-compatible
  syntax even if native trusted source handlers themselves use PCRE2.
- Inheriting `remove`, `skipIfAlreadyFound`, `skipFromTitle`, and
  other native options is mandatory; the learner cannot set them itself.
- The general ADD path is restricted to trusted fixed-value and boolean
  native handlers that skip already populated fields; it cannot silently
  replace a nonempty value. Lists, dates, free-text and arbitrary numeric
  transforms use other verified paths or remain review-only.
- Unknown words appearing inside film titles, metadata aliases, release
  groups and private strings must not be promoted based on the parser's
  own answer. The public vocabulary is a **second acceptance gate**.
- The validation corpus is bounded, so regression checks can reduce
  risk but **cannot mathematically guarantee no future regression**.
  The activation threshold and independent local history protect against
  blindly accumulating rules.

A verified model proposal may therefore remain **review-only** for any
field, not because that field was excluded, but because the available
data did not demonstrate a safe, measurable improvement.

## Live rule lifecycle

1. Observe raw titles and failures on the background worker only.
2. Store bounded local evidence in SQLite, keyed by rule syntax, native
   handler index and a hashed title identity; no repeated title may
   impersonate independent identities.
3. Generate a constrained regex for an existing native transform.
4. Compare the old parser, existing learned rules and the proposed
   *complete* replacement/addition engine on positive, negative,
   cross-field, bundled real-release smoke cases and counterfactual examples.
   Persisted rule syntax and source identity must pass independent validation
   at every reconciliation; missing or invalid evidence retires active rules.
5. Promote only when the candidate adds real repairs with no observed
   regressions. Reject syntactic/semantic duplicates and no-ops.
6. Compile the proposed generation **before** modifying persisted rules.
   Publish only after the local materialization succeeds, using a single
   immutable parser generation swap. A storage or compilation failure must
   not introduce an unrecorded live regex.
7. Persist accepted rules and their measured repairs locally.
8. After parser upgrades, reparse original local evidence against the
   new canonical engine before restoring a learned rule. Retire rules
   that became unnecessary or conflict with updated native behavior.
9. Optionally export *review-only*, strict-grammar regex proposals and
   aggregate evidence via the GitHub community workflow. Only a fixed,
   audited public marker vocabulary is exportable from the general learner.
   Raw release names and full parsed identities are never part of a proposal.

## Performance and operations

- Rust only; no Ollama, external model server, GPU or Python runtime.
- No additional per-request regex parsing/provenance: the full raw match
  is captured only when background training explicitly requests it.
- For each training title, consider at most **one** original source handler
  and three bounded punctuation/number variants per handler.
- Store no more than 16,384 general counterfactual examples, further
  restricted by `MATCHLAB_ML_TRAINING_MAX_EXAMPLES`; prefer evicting
  unpromoted evidence over evidence behind verified rules.
- Compile candidate rule sets only in the background. Rule revision
  changes invalidate parser cache automatically.
- `GET /api/ml/status` includes `general_regex`, with native field
  inventory, active rules, evidence volume and replay rejection counts.

## Threat model and practical correctness guarantee

Training is influenced by raw release names, which are **untrusted input**.
Distinct-title hashes limit ordinary repetition but cannot prove that
several fabricated titles came from independent publishers. Attackers can
attempt data poisoning, regex collisions and false release metadata labels.
Mitigations: limited regex grammar, a fixed safe vocabulary, immutable
native source transforms, bounded worker/replay resource usage, held-out
regressions, local-only raw data, and a fail-closed compiler for unverified
higher-priority corrections.

No finite corpus can guarantee that a newly installed ADD will never
match an unseen ordinary title by accident. We therefore claim
**zero observed regressions against the validated corpus**, NOT
mathematical zero regression against every possible future release.
Operators requiring strict zero change can set
`MATCHLAB_ML_LIVE_REGEX_ENABLED=false`. This preserves background
training and evidence while ensuring the native live parser does not
install learned regex generations. Alternatively
`MATCHLAB_ML_ENABLED=false` disables local ML altogether. Both
controls require a container restart when changing Docker environment
variables. The ordinary canonical parser and matcher remain functional
without learned regexes. The separate verified AniDB episode-hint
feature is outside this native-regex publication switch.

## Remaining work before declaring universal correctness

All fields are **eligible for general learning** through the same
source-aware regex pipeline, but not every field has an independent
semantic oracle for arbitrary new strings. For comprehensive automatic
synonym corrections, malformed group names, IMDb title/year identity,
and ambiguous multilingual releases, implement additional trusted
reference providers with targeted negative corpora. Do not weaken
the replay and no-duplicate gates to increase the number of active rules.
