---
name: alsp
description: Answers questions about Rust code through the alsp command line: instant syn-powered file and dependency-crate outlines, batched edit/refactor prep briefs, type-resolved callers (including enum variants), tests that reach a symbol, observed coverage, predicted test gaps, and diff review, each fact marked by how it was found. Use instead of reading whole large .rs files, grepping ~/.cargo/registry for dependency APIs, or running grep/git grep in a Rust repository (one with a Cargo.toml) when orienting in a file or crate, inspecting a third-party dependency crate, preparing to edit or refactor symbols, finding who calls or uses a function or enum variant, checking where a symbol is defined, checking whether it is tested or covered, finding which tests a change affects or what breaks if it changes, or reviewing a diff. Not for code in any other language.
---

# alsp

`alsp` answers Rust code questions from instant `syn` outlines, a rust-analyzer index, and test/coverage measurements. Use `alsp outline` instead of reading whole large `.rs` files or grepping `~/.cargo/registry`; use `alsp prep` before editing or refactoring symbols; and use `alsp refs`/`tests`/`sym` instead of grep for callers, enum variants, test reach, and coverage: grep cannot resolve types, trait methods, or re-exports, and matches names in comments, strings, and unrelated items of the same name. Run it from inside the repository, or pass `--repo <path>`.

## Pick the command

| Job / Question | Command |
|---|---|
| Orient in a large `.rs` file or crate (instead of `Read` or `grep "pub fn\|struct\|enum"`) | `alsp outline <paths...> [--pub]` |
| Inspect a third-party dependency crate's API (instead of grepping `~/.cargo/registry`) | `alsp outline --dep <crate> [--pub]` |
| Prepare to edit or refactor 1+ symbols (signature, collisions, callers, impls, tests in 1 call) | `alsp prep <targets...>` (default 1500-token budget; `--budget <n>` or `--all`) |
| Find uses of an enum variant | `alsp refs Enum::Variant` |
| Where is `X` defined? | `alsp sym X` |
| Who calls or uses `X`? | `alsp refs X` |
| What breaks if I change `X`? What does this change affect? | `alsp prep X` (or `alsp refs X` then `alsp tests X`) |
| Which tests break or need rerunning if I change `X`? Is `X` tested? | `alsp tests X` (add `--run` to see whether the package's tests execute it) |
| How much of `X` do tests run? | `alsp cover X` |
| Which bugs in `X` would tests miss? | `alsp gaps X` |
| What does this diff touch? | `alsp pack --base <rev> [--head <rev>]` |
| Review my changes | `alsp review` |
| Is alsp set up? Something is missing | `alsp doctor` |

`X` is a name (`handle_request`, `net::handle_request`, or an enum variant `Status::Ready`) or `file:line`. A name that matches several definitions makes `sym` list them all; every other indexed command exits 4 and prints candidates, so repeat with a longer name or `file:line`.

Run `alsp doctor` when a command fails for a missing tool: it lists each prerequisite with the command that fixes it, and its last line is `alsp doctor: ready` when `alsp`, `git` and `rust-analyzer` work.

Start with `alsp outline` to orient in unfamiliar files or crates, `alsp prep` before editing 1+ symbols, `alsp sym` when unsure of a name, `alsp refs` for impact, `alsp tests` before claiming something is tested, and `alsp cover` or `gaps` only when the question is about test quality.

Useful flags on indexed queries: `--json` (stable schema), `--all` (lifts list caps and `prep`'s token budget), `--repo <path>`, `--no-build` (see Trust and cost; `review` has none), and `--root <dir>` to also index a Cargo package outside the root workspace (for example `--root apps/foo/rust`; the answer says when it is needed). `alsp outline` takes optional `[paths...]` (default `src/` or `.`), `--pub`, `--dep <crate>`, `--repo <path>`, and `--json`.

## Read the answer

Each section line ends with how its facts were found:

- `[resolved]`: from the rust-analyzer index (or `syn` AST for `outline` and `prep` declarations); type/syntax-resolved, treat as fact.
- `[observed]`: measured by running tests (coverage).
- `[predicted]`: estimated by a model (`gaps`); a lead to check, not a verdict.
- trailing `~`: heuristic (for example `pub` standing in for "reachable from outside").

Lines beginning `!` are notes: a dirty working tree, a root that failed to index, a measurement that is missing. Read them before quoting a result. When `refs` and `tests` come back empty with exit 0, the symbol is unused by resolved references, though macro-generated code and calls through function pointers or macros can be missed.

`refs`, `tests`, and `prep` on a trait method also include the call sites and tests of its impls, and on an impl method the sites that call through the trait. Those links are heuristic: groups end `via <A as T>::m` (or `via T::m`) and carry `~`, because a call through the trait may run any impl. Check them before relying on them.

## Exit codes

| Code | Meaning | Do |
|---|---|---|
| 0 | Answered | Use the answer |
| 2 | Usage error, not a git work tree, no `Cargo.toml`, invalid `outline` path / symlink escape / syntax error / unknown `--dep` | Fix the command, path, or directory |
| 3 | No index could be served | An empty result means unknown, not none. `alsp review` and `alsp pack` exit 3 too: they still print what they have, but evidence is unknown, not absent (an empty symbol list means nothing was checked). On code you trust, retry without `--no-build` (that builds the index, which runs repository code), or add `--root` |
| 4 | Target not found or ambiguous (for `prep`, all resolved targets are still printed alongside the errors) | Pick from the printed candidates or nearest names. If you know the symbol exists, it may sit in a Cargo package outside the root workspace: add `--root <package dir>` (for example `--root apps/foo/rust`) |
| 5 | A measurement is unavailable (the package does not build, sdd `decide` missing) | The partial answer is printed; its notes say why. Failing tests do not cause this: coverage is still measured, exit 0, and a note names the failed tests and the first panic; fix the cause (often a missing environment variable) and rerun |

## Trust and cost

**Local `alsp outline` executes no repository code:** given local paths (without `--dep`), it reads `.rs` files directly and parses them with `syn` (no `rust-analyzer`, no `cargo`, refuses symlinks escaping the repo root and symlink loops), so it is safe on untrusted code and runs in milliseconds. By contrast, `alsp outline --dep <crate>` runs `cargo metadata` in the repository and is **not** safe on untrusted code.

Building the index runs the repository's build scripts and proc macros, so every indexed command without `--no-build` executes repository code (`review` has no `--no-build`, so it always can). The first index of a large repo takes about 30 s; later queries reuse the cache.

`--no-build` only stops indexing and serves a cached index (exit 3 if none). It is safe on untrusted code for `sym`, `refs`, `tests` without `--run`, `prep`, and `pack` without `--with`, and nothing else. `cover`, `gaps`, `tests --run`, `pack --with` and `review --full` or `--with` build and run the repository's tests, and `--no-build` does not stop that. Run those only on code you trust. `gaps` also sends function bodies to Google's Gemini through sdd; confirm before spending on `--full` or `gaps` across many symbols.

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

- "What's in `src/engine.rs`?" -> `alsp outline src/engine.rs --pub` to inspect its types, traits, and signatures with line spans before reading specific ranges.
- "How do I use `serde_json` here?" -> `alsp outline --dep serde_json --pub` instead of grepping `~/.cargo/registry`.
- "Refactor `net::handle_request` and `Status::Ready`" -> `alsp prep net::handle_request Status::Ready` to get both declarations, collisions, callers, links, and tests in one call.
- "Who calls `handle_request`?" -> `alsp refs handle_request`; report the grouped callers and entry points, marking `[test]` callers separately.
- "Is `net::dispatch` tested?" -> `alsp tests net::dispatch --run`; `executed: no` with no listed tests is a real gap, `unknown` is not.
- "Review what I just changed" -> `alsp review --quick` (on code you trust), then read the changed symbols' callers before judging risk.
