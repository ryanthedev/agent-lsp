---
name: alsp
description: Answers questions about Rust code through the alsp command line, which gives type-resolved callers, tests that reach a symbol, observed coverage, predicted test gaps and diff review, each fact marked by how it was found. Use instead of grep or git grep when asked in a Rust repository (one with a Cargo.toml) who calls or uses a function, where a symbol is defined, whether it is tested or covered, which tests a change affects or what breaks if it changes, or to review a diff or recent changes. Not for code in any other language.
---

# alsp

`alsp` answers Rust code questions from a rust-analyzer index and measurements. Use it instead of grep for callers, test reach and coverage: grep cannot resolve types, trait methods or re-exports, and it also matches names in comments, strings and unrelated items of the same name. Run it from inside the repository, or pass `--repo <path>`.

## Pick the command

| Question | Command |
|---|---|
| Where is `X` defined? | `alsp sym X` |
| Who calls or uses `X`? | `alsp refs X` |
| What breaks if I change `X`? What does this change affect? | `alsp refs X` (callers, entry points), then `alsp tests X` (tests that reach it) |
| Which tests break or need rerunning if I change `X`? Is `X` tested? | `alsp tests X` (add `--run` to see whether the package's tests execute it) |
| How much of `X` do tests run? | `alsp cover X` |
| Which bugs in `X` would tests miss? | `alsp gaps X` |
| What does this diff touch? | `alsp pack --base <rev> [--head <rev>]` |
| Review my changes | `alsp review` |
| Is alsp set up? Something is missing | `alsp doctor` |

`X` is a name (`handle_request`, or `net::handle_request`) or `file:line`. A name that matches several definitions makes `sym` list them all; every other command exits 4 and prints candidates, so repeat with a longer name or `file:line`.

Run `alsp doctor` when a command fails for a missing tool: it lists each prerequisite with the command that fixes it, and its last line is `alsp doctor: ready` when `alsp`, `git` and `rust-analyzer` work.

Use `alsp sym` first when unsure of the name, `alsp refs` for impact, `alsp tests` before claiming something is tested, and `alsp cover` or `gaps` only when the question is about test quality.

Useful flags on every query: `--json` (stable schema), `--all` (lists are capped at 25), `--repo <path>`, `--no-build` (see Trust and cost; `review` has none), and `--root <dir>` to also index a Cargo package outside the root workspace (for example `--root apps/foo/rust`; the answer says when it is needed).

## Read the answer

Each section line ends with how its facts were found:

- `[resolved]`: from the rust-analyzer index; type-resolved, treat as fact.
- `[observed]`: measured by running tests (coverage).
- `[predicted]`: estimated by a model (`gaps`); a lead to check, not a verdict.
- trailing `~`: heuristic (for example `pub` standing in for "reachable from outside").

Lines beginning `!` are notes: a dirty working tree, a root that failed to index, a measurement that is missing. Read them before quoting a result. When `refs` and `tests` come back empty with exit 0, the symbol is unused by resolved references, though macro-generated code and calls through function pointers or macros can be missed.

`refs` and `tests` on a trait method also include the call sites and tests of its impls, and on an impl method the sites that call through the trait. Those links are heuristic: groups end `via <A as T>::m` (or `via T::m`) and carry `~`, because a call through the trait may run any impl. Check them before relying on them.

## Exit codes

| Code | Meaning | Do |
|---|---|---|
| 0 | Answered | Use the answer |
| 2 | Usage error, not a git work tree, no `Cargo.toml` | Fix the command or directory |
| 3 | No index could be served | An empty result means unknown, not none. `alsp review` and `alsp pack` exit 3 too: they still print what they have, but evidence is unknown, not absent (an empty symbol list means nothing was checked). On code you trust, retry without `--no-build` (that builds the index, which runs repository code), or add `--root` |
| 4 | Target not found or ambiguous | Pick from the printed candidates or nearest names. If you know the symbol exists, it may sit in a Cargo package outside the root workspace: add `--root <package dir>` (for example `--root apps/foo/rust`) |
| 5 | A measurement is unavailable (the package does not build, sdd `decide` missing) | The partial answer is printed; its notes say why. Failing tests do not cause this: coverage is still measured, exit 0, and a note names the failed tests and the first panic; fix the cause (often a missing environment variable) and rerun |

## Trust and cost

Building the index runs the repository's build scripts and proc macros, so every command without `--no-build` executes repository code (`review` has no `--no-build`, so it always can). The first index of a large repo takes about 30 s; later queries reuse the cache.

`--no-build` only stops indexing and serves a cached index (exit 3 if none). It is safe on untrusted code for `sym`, `refs`, `tests` without `--run`, and `pack` without `--with`, and nothing else. `cover`, `gaps`, `tests --run`, `pack --with` and `review --full` or `--with` build and run the repository's tests, and `--no-build` does not stop that. Run those only on code you trust. `gaps` also sends function bodies to Google's Gemini through sdd; confirm before spending on `--full` or `gaps` across many symbols.

alsp's git calls neutralize the repository's own git config (exceptions in docs/query.md).

Authority and detail: [docs/query.md](../../docs/query.md), "Trust and cost".

## Review a diff

`alsp review` with no arguments reviews everything since the last review, uncommitted work included. It prints its cost to stderr first.

- `--quick`: mechanical evidence only (changed symbols, callers, entry points); no agents, no tests run. On code you trust, use this by default from an agent: it still builds the index.
- default tier: also runs the agents listed in `.alsp/enabled`, if any; this uses agent quota and takes minutes.
- `--full`: adds coverage and predicted gaps; runs the repository's tests and costs Gemini spend.
- `--base <rev>`, `--head <rev>`, `--agent codex|claude|agy`, `--untracked`, `--print` (emit the prompt and pack for another agent), `--format json`.

Findings are appended to `.alsp/findings.jsonl`. Record whether one held with `alsp ledger mark <id> held|not-held`; inspect with `alsp ledger list [--unmarked]` and `alsp ledger stats`. Agent findings are claims to verify in the code before acting. Details: [docs/review.md](../../docs/review.md).

## Examples

- "Who calls `handle_request`?" -> `alsp refs handle_request`; report the grouped callers and entry points, marking `[test]` callers separately.
- "Is `net::dispatch` tested?" -> `alsp tests net::dispatch --run`; `executed: no` with no listed tests is a real gap, `unknown` is not.
- "Review what I just changed" -> `alsp review --quick` (on code you trust), then read the changed symbols' callers before judging risk.
