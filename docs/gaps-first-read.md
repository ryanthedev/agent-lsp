# Predicted test gaps on Rust: a first read

A first measurement of `alsp pack --with coverage,gaps` on a private Rust application, 2026-11-14. It records a number and has no pass bar. The measured code is private, so the table below is anonymized: file names, functions and lines are stand-ins, and the reasons keep only the shape of each judgment. The method and the counts are the real ones. The table also serves as sample input for `alsp ledger import-gaps docs/gaps-first-read.md`, which reads rows of this shape (number, item, `file:line`, probability, verdict, reason, confidence) into the findings ledger.

**Result: 5 of the top 10 predicted gaps are real test gaps** (the other 5 would be caught by a test that already exists).

No mutant was executed. The verdicts come from a reviewer (a coding agent) reading the mutated code and the tests that reach it. They are reading verdicts, not run results, so each carries a confidence.

## Setup

- Command: `alsp pack --repo <repo> --base <c>~1 --head <c> --with coverage,gaps --gap-budget 250 --format json`, for three commits.
- Mutants: `cargo mutants --list --diff --json --file=<changed file>` (cargo-mutants 27.1.0, list only). Coverage: `cargo llvm-cov --lcov --branch -p <package>` (cargo-llvm-cov 0.9.1).
- Model: Google's Gemini decision model, prompted with the enclosing item with the mutation marked, plus the `#[test]` functions the index says reach it, and the question "Would the existing tests fail if this change were made?". A gap is reported when P(tests catch) < 0.50.
- Data egress: function and test bodies of the private application were sent to Google's API.

## What each commit produced

| Commit | Changed symbols | Coverage | Mutants listed | Gaps |
|---|---|---|---|---|
| A | 3 test functions | not measured (`cargo llvm-cov` excludes `tests/`) | 0 (cargo-mutants never mutates test code) | 0 |
| B | 1 test function | not measured (same reason) | 0 | 0 |
| C | 73 symbols in two library crates | 17 symbols with line and branch coverage | 670 | 69 (58 predicted survivors, 11 uncovered) |

For commit C: 670 mutants listed, 470 of them in unchanged parts of the changed files (not attributed to any changed item), 11 on lines no test runs (reported with no model call), 189 sent to the model, 185 answered, 4 unanswered (HTTP 429: the batch of about 1.8M tokens was sent faster than the account's input-token rate limit). 1,811,136 input tokens, $0.1811 for the last run. Live spend for four runs: $0.46 in all.

The 11 uncovered gaps are all real by observation: match arms and a fail-closed reset path that no test reaches.

## Top 10 predicted survivors (all in commit C)

Ranked by lowest P(tests catch). "Real gap" means no existing test would fail with this mutation.

| # | Item and mutation | File:line (col) | P(catch) | Verdict | Reason (one line) | Conf. |
|---|---|---|---|---|---|---|
| 1 | `Text::measure`: `*` to `+` | `src/text.rs:205` (32) | 0.2015 | real gap | The fallback size computation runs only for a node with no runs; every test gives runs, and branch coverage shows the arm unreached. | high |
| 2 | `Store::apply`: `>` to `<` | `src/store.rs:821` (29) | 0.2092 | not a gap | The fail-closed size guard would be true after nearly every patch, resetting the store; every applied-patch assertion fails. | high |
| 3 | `Text::measure`: `+=` to `*=` | `src/text.rs:202` (15) | 0.2140 | real gap | Width becomes 0 for every node; nothing in the tests reads the reported width. | medium |
| 4 | `parse_path`: `>` to `==` | `src/text.rs:232` (16) | 0.2151 | not a gap | The length cap would stop firing; one test feeds an over-long but otherwise valid path and asserts it is rejected. | high |
| 5 | `Store::apply`: `or` to `and` | `src/store.rs:789` (45) | 0.2176 | not a gap | A move with one side missing would reach an unwrap on a missing key; a test covers exactly that case and would panic. | high |
| 6 | `Text::measure`: `*` to `/` | `src/text.rs:205` (32) | 0.2265 | real gap | Same unreached no-runs fallback as row 1. | high |
| 7 | `Store::apply`: delete match arm `"props"` | `src/store.rs:684` (17) | 0.2266 | not a gap | The operation would fall to the catch-all arm and panic; two tests apply `props` operations. | high |
| 8 | `Text::measure`: `+=` to `-=` | `src/text.rs:202` (15) | 0.2276 | real gap | Width goes negative (clamped to 0 when drawn); unobserved for the same reason as row 3. | medium |
| 9 | `Store::check`: `+` to `*` in a depth sum | `src/store.rs:352` (37) | 0.2294 | real gap | The depth cap on moves is off by one; the only depth test uses inserts, and no test moves a node near the cap. | high |
| 10 | `budget`: `+` to `-` in a byte cost | `src/text.rs:97` (48) | 0.2414 | not a gap | The subtraction underflows `usize` (a debug-build panic) for any short text, which several tests use. | high |

Count of real gaps: **5 of 10** (rows 1, 3, 6, 8, 9). Rows 1, 3, 6 and 8 are one weakness: the tests never check the size the measuring function reports. Row 9 is an untested boundary.

## Observations

- The model's P(catch) for the top 14 sits in a narrow band, 0.20 to 0.26, so rank within the band carries little information. Five of its top 10 are mutations a test would catch; three of them turn a guard or an arm into a reset or a panic, which the model does not see.
- The mutated function was a trait impl method. The index does not attribute calls made through a trait, so it had no reaching tests, and the prompt said so. Six of the top 14 gaps are in it. That lowers P(catch) for the whole function and is a known bias of the index, not of the model.
- Two runs of the same input gave different top lists (model nondeterminism plus the dropped requests), so a single list is a sample, not a fixed ranking.
- The verdicts for rows 3 and 8 rest on "nothing observes the width"; running the mutants would settle them. They are the least certain rows.
