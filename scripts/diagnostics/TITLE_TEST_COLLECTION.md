# Collecting title tests

`collect_title_tests.py` reads explicit Rust test assertions without running
source code or calling MatchLab. It uses Python's standard library only:

```sh
python3 scripts/diagnostics/collect_title_tests.py --output-dir /tmp/matchlab-title-audit
```

At the reviewed source revision, its outputs contain:

- `parser-title-expectations.json`: 400 distinct inputs from 119 parser tests;
  378 inputs have 913 machine-readable source assertions. Other source
  predicates remain in `assertions_manual`.
- `release-300-parser-expectations.json`: 300 generated movie/TV/anime release
  variants and 11 committed TV titles, with 722 source assertions.
- `source-test-inventory.json`: all 122 test functions in these source files,
  their locations and any loops this restricted collector could not expand.

These are regression expectations. They do not independently establish IMDb or
AniDB identity, and partial assertions do not prove that every returned field is
correct. For example, a tentative anime hint with `verified: false` is an
intentional outcome. The collector skips conditional branches and unsupported
expressions rather than manufacturing unconditional labels. Its small mirror of
`release_300::variants` fails closed if that source function changes.

## Inventory every executed input

An optional trace also captures generated inputs and calls in server tests.
Instrumentation belongs in a separate disposable worktree. The collector can
emit its patch without modifying the selected checkout:

```sh
python3 scripts/diagnostics/collect_title_tests.py \
  --output-dir /tmp/matchlab-title-audit \
  --trace-patch /tmp/matchlab-title-audit/trace.patch

git worktree add --detach /tmp/matchlab-title-trace HEAD
git -C /tmp/matchlab-title-trace apply /tmp/matchlab-title-audit/trace.patch

MATCHLAB_DIAGNOSTIC_TITLE_TRACE=/tmp/matchlab-title-audit/trace.jsonl \
  RUSTFLAGS='-D warnings' \
  cargo test --manifest-path /tmp/matchlab-title-trace/Cargo.toml --workspace --locked

python3 scripts/diagnostics/collect_title_tests.py \
  --output-dir /tmp/matchlab-title-audit \
  --trace /tmp/matchlab-title-audit/trace.jsonl
```

Use a fresh trace file for each run and retain its test log and source revision.
Only compare outputs from a passing test run. The generated trace patch records
public parses, batch inputs and internal engine stages, including thread name,
caller, language translation flag and canonical/live/replay mode. Caller line
numbers refer to the instrumented worktree. Tracing adds work and is unsuitable
for timing or production use.

`executed-title-inventory.json` lists every traced input, including intermediate
path components. `title-parity-corpus.json` contains only stable public outputs
from the canonical parser or an empty live-rule generation. It excludes replay
outputs, internal-only components, translated results and API-invalid empty or
oversized inputs. Inconsistent baseline variants remain excluded and reported.
`title-parity-new-only.json` additionally excludes the 400 parser golden inputs;
its remaining cases can overlap the separately labeled release-300 cohort.

**Parity data are local observations, not correctness labels.** The corpus marks
them `local_remote_differential`, which `audit_api.py` reports as reference
agreement separately from source-assertion results. The input inventory is the
complete executed sample; the restricted static expectations cover a subset.
