# alsp

`alsp` is a command-line tool that gives coding agents evidence about Rust code. An agent asked "what's in this file?", "who calls this?", or "is this tested?" usually reads whole files or greps and guesses. `alsp` answers from instant `syn`-powered file and dependency-crate outlines (`outline`), a rust-analyzer index (batched edit briefs via `prep`, plus type-resolved definitions, references including enum variants, and tests), and measurements (coverage from `cargo llvm-cov`, mutants from `cargo mutants --list` scored by a decision model), and it marks every fact with how it was found. It is a plain program with text and JSON output, so Claude Code, Codex and agy use it the same way, and a plugin skill teaches each of them when to reach for it.

## What you need

- A Rust toolchain, `rust-analyzer` and `git` 2.31 or newer. `alsp` refuses an older git.
- `cargo-llvm-cov` for `cover`, `tests --run` and coverage in reviews.
- `cargo-mutants` for `gaps`, plus a clone of [sdd](https://github.com/ryanthedev/sdd) (its `decide` command, `src/cli/decide.ts`, run with [bun](https://bun.sh)), which scores mutants with Google's Gemini. `alsp` finds the clone through `ALSP_SDD_ROOT` (default `~/repos/sdd`) and bun through `ALSP_BUN` (default: `bun` on `PATH`, else `~/.bun/bin/bun`). `alsp` holds no API key; sdd does: see the "Decision calls without MCP" section of sdd's README for `GEMINI_API_KEY` and the budget defaults. Without sdd, `gaps` still lists the mutants on lines no test runs, reports the model as unavailable and exits 5; the mutants on covered lines get no score.
- Optional, for `alsp review --agent`: the `claude`, `codex` or `agy` CLI.

## Install

```
curl -fsSL https://raw.githubusercontent.com/ryanthedev/agent-lsp/main/scripts/install.sh | sh
alsp doctor
```

The script builds `alsp` with `cargo install`, adds `rust-analyzer` through rustup if it is missing, and installs the plugin for each of Claude Code, Codex and agy that it finds. None of them is required. Re-run it to update. `alsp doctor` then lists anything still missing, each with the command that fixes it. Read the script first if you like, or preview it with `sh scripts/install.sh --dry-run`; [docs/install.md](docs/install.md) has the options and how to undo each step.

## Quick start

Run these inside a Rust repository. The first query of a large repository builds the index (about 30 s on a big workspace) and later ones reuse it. This output is from `alsp` run on its own source:

```
$ alsp sym split_markdown_row
sym index=missing+rebuilt @45c0c3a6
matches(1) [resolved]
  fn split_markdown_row crates/alsp-review/src/lib.rs:620 refs=3

$ alsp refs split_markdown_row
refs split_markdown_row index=fresh @45c0c3a6
target fn crates/alsp-review/src/lib.rs:620-628 [resolved]
refs(3 in 2 symbols) [resolved]
  import_gaps crates/alsp-review/src/lib.rs:633
  tests::split_markdown_row_handles_escaped_pipes_and_edges crates/alsp-review/src/lib.rs:958,959 [test]
entry(5) [resolved+heuristic]
  test tests::split_markdown_row_handles_escaped_pipes_and_edges crates/alsp-review/src/lib.rs:957
  pub import_gaps crates/alsp-review/src/lib.rs:630~
  ...

$ alsp tests split_markdown_row
tests(2) [resolved]
  tests::split_markdown_row_handles_escaped_pipes_and_edges crates/alsp-review/src/lib.rs:957 direct
  tests::test_DW_4_7_imports_the_ten_historical_gap_verdicts crates/alsp-review/src/lib.rs:827 indirect

$ alsp outline crates/alsp-review/src/lib.rs --pub   # instant syn skeleton (or --dep <crate>)
$ alsp prep split_markdown_row                       # batched declaration, collisions, callers, links, tests
$ alsp cover split_markdown_row     # runs the package's tests under cargo llvm-cov
$ alsp review --quick               # evidence for everything changed since the last review
```

`alsp` with no arguments prints a one-screen cheat sheet meant for agents. `alsp refs Enum::Variant` finds uses of an enum variant. `alsp gaps <symbol>` lists the mutants in a symbol and the predicted chance the tests catch each.

## Provenance marks

Every section says how its facts were obtained:

| Mark | Meaning |
|---|---|
| `[resolved]` | From the rust-analyzer index, or from syntax that leaves no doubt. Treat as fact. |
| `[observed]` | Measured by running the tests (coverage). |
| `[predicted]` | Estimated by a model (`gaps`). A lead to check, not a verdict. |
| trailing `~` | A heuristic, for example `pub` standing in for "reachable from outside the crate". |

Lines starting with `!` are notes (dirty working tree, a root that failed to index, a missing measurement). Read them before quoting a result.

## Use it from an agent

The plugin is one skill, `skills/alsp/SKILL.md`, that teaches Claude Code, Codex and agy which command answers which question and how to read the marks. The install script above sets it up; [docs/plugin.md](docs/plugin.md) has the by-hand commands. In Claude Code alone: `/plugin install alsp --marketplace ryanthedev/agent-lsp`.

## Trust and cost

Local `alsp outline` (without `--dep`) reads `.rs` files directly and parses them with `syn` (symlink-guarded, no `cargo`, no `rust-analyzer`), so it is safe on untrusted code; `alsp outline --dep <crate>` runs `cargo metadata` and is not. Indexing runs the repository's build scripts and proc macros, because rust-analyzer does. `cover`, `gaps`, `tests --run` and `review --full` also build and run its tests, and `gaps` sends function bodies to Google's Gemini API. Point `alsp` only at code you trust; `--no-build` makes `sym`, `refs`, `tests` (without `--run`), and `prep` safe on untrusted code, but only against a cached index. Review agents run in a throwaway export of the commit, which is not a sandbox. The details are in [docs/query.md](docs/query.md), "Trust and cost", and [docs/review.md](docs/review.md).

## Documentation

- [docs/query.md](docs/query.md): symbol queries, output formats, JSON keys, exit codes, trust and cost.
- [docs/review.md](docs/review.md): diff review, agents, the findings ledger and how well findings held up.
- [docs/install.md](docs/install.md): the install script, its options, and undoing it.
- [docs/doctor.md](docs/doctor.md): `alsp doctor`, what it checks.
- [docs/plugin.md](docs/plugin.md): the plugin files and installing the skill by hand.
- [docs/gaps-first-read.md](docs/gaps-first-read.md): a first measurement of predicted test gaps.
- [CONTRIBUTING.md](CONTRIBUTING.md) and [AGENTS.md](AGENTS.md): building and changing `alsp`.

## Status and limits

Early. Rust only. There is no daemon and no editing: `alsp` reads and measures. Calls through trait objects, function pointers or macros are not followed, so an empty answer means "no resolved references", not "unused". Predicted gaps come from a model reading code; in one measurement about half of the top predictions were real gaps (docs/gaps-first-read.md).

## License

MIT. See [LICENSE](LICENSE).
